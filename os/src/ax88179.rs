extern crate alloc;

use crate::error::Error;
use crate::error::Result;
use crate::executor::TimeoutFuture;
use crate::memory::Mmio;
use crate::print::hexdump;
use crate::println;
use crate::xhci::controller::EndpointType;
use crate::xhci::device::UsbDeviceDriverContext;
use crate::xhci::future::EventFuture;
use crate::xhci::trb::DataStageTrb;
use crate::xhci::trb::SetupStageTrb;
use crate::xhci::trb::StatusStageTrb;
use alloc::boxed::Box;
use alloc::format;
use core::pin::Pin;

// References:
// https://elixir.bootlin.com/linux/v6.0.6/source/include/uapi/linux/mii.h
// https://github.com/gomesjj/ax88179_178a/blob/983769c22e887e244662197ba7aec24fa6fbed94/ax88179_178a.h
// https://github.com/lwhsu/if_axge-kmod/blob/be7510b1bc7e5974963fe17e67462d3689e80631/if_axgereg.h

// MMI: Media Independent Interface
const MII_BMCR: u16 = 0x00; // Control
const MII_BMSR: u16 = 0x01; // Status

const GMII_PHY_PHYSR: u16 = 0x0011;
// const GMII_PHY_PHYSR_GIGA: u16 = 0x8000;
// const GMII_PHY_PHYSR_100:  u16 = 0x4000;
// const GMII_PHY_PHYSR_FULL: u16 = 0x2000;
// const GMII_PHY_PHYSR_LINK: u16 = 0x0400;

const BMCR_ANRESTART: u16 = 0x0200; // Restart auto negotiation
const BMCR_ANENABLE: u16 = 0x1000; // Supporting auto negotiation

const REQUEST_ACCESS_MAC: u8 = 0x01;
const REQUEST_ACCESS_PHY: u8 = 0x02;

const MAC_REG_PHYS_LINK_STATUS: u16 = 0x0002;
// const MAC_REG_PHYS_LINK_STATUS_USB_SS: u8 = 0x04;
// const MAC_REG_PHYS_LINK_STATUS_USB_SS: u8 = 0x04;

const MAC_REG_RX_CTL: u16 = 0x000b;
const MAC_REG_RX_CTL_PROMISCUOUS: u16 = 0x0001;
const MAC_REG_RX_CTL_ACCEPT_ALL_MULTICAST: u16 = 0x0002;
const MAC_REG_RX_CTL_ACCEPT_BROADCAST: u16 = 0x0008;
const MAC_REG_RX_CTL_ACCEPT_MULTICAST: u16 = 0x0010;
const MAC_REG_RX_CTL_ACCEPT_PHYS_ADDR_FROM_MULTICAST_ARRAY: u16 = 0x0020;
// https://github.com/lamw/ax88179_178a-esxi/blob/master/ax88179_178a.h#L55
// HW auto-added 8-bytes data when meet USB bulk in transfer boundary (1024/512/64)
// const MAC_REG_RX_CTL_HA8B: u16 = 0x0080;
const MAC_REG_RX_CTL_START: u16 = 0x0080;
const MAC_REG_RX_CTL_DROP_CRC_ERROR: u16 = 0x0100;
const MAC_REG_RX_CTL_ALIGN_IP_HEADER: u16 = 0x0200;

const MAC_REG_MEDIUM_STATUS_MODE: u16 = 0x0022;
const MAC_REG_MEDIUM_GIGAMODE: u16 = 0x0001;
const MAC_REG_MEDIUM_FULL_DUPLEX: u16 = 0x0002;
// const MAC_REG_MEDIUM_ALWAYS_ONE: u16 = 0x0004;
const MAC_REG_MEDIUM_RXFLOW_CTRLEN: u16 = 0x0010;
const MAC_REG_MEDIUM_TXFLOW_CTRLEN: u16 = 0x0020;
const MAC_REG_MEDIUM_RECEIVE_EN: u16 = 0x0100;
// const MAC_REG_MEDIUM_PS: u16 = 0x0200;

const MAC_REG_MONITOR_MODE: u16 = 0x0024;
// const MAC_REG_MONITOR_MODE_RWLC: u8 = 0x02;
const MAC_REG_MONITOR_MODE_RWMP: u8 = 0x04;
// const MAC_REG_MONITOR_MODE_RWWF: u8 = 0x08;
// const MAC_REG_MONITOR_MODE_RW_FLAG: u8 = 0x10;
const MAC_REG_MONITOR_MODE_PMEPOL: u8 = 0x20;
const MAC_REG_MONITOR_MODE_PMETYPE: u8 = 0x40;

const MAC_REG_PHYPWR_RSTCTL: u16 = 0x0026;
const MAC_REG_PHYPWR_RSTCTL_IPRL: u16 = 0x0020;

const MAC_REG_RX_BULK_IN_QUEUE_CFG: u16 = 0x2e;

const MAC_REG_CLK_SELECT: u16 = 0x0033;
const MAC_REG_CLK_SELECT_BCS: u8 = 0x01;
const MAC_REG_CLK_SELECT_ACS: u8 = 0x02;

const MAC_REG_RX_CSUM_OFFLOADING_CTL: u16 = 0x0034;
const MAC_REG_TX_CSUM_OFFLOADING_CTL: u16 = 0x0035;

const AX_CSUM_OFFLOADING_IP: u8 = 0x01;
const AX_CSUM_OFFLOADING_TCP: u8 = 0x02;
const AX_CSUM_OFFLOADING_UDP: u8 = 0x04;
const AX_CSUM_OFFLOADING_TCPV6: u8 = 0x20;
const AX_CSUM_OFFLOADING_UDPV6: u8 = 0x40;

const MAC_REG_PAUSE_WATERLVL_HI: u16 = 0x0054;
const MAC_REG_PAUSE_WATERLVL_LO: u16 = 0x0055;

// AXGE_NODE_ID
const MAC_REG_NODE_ID: u16 = 0x0010;

async fn read_from_device<T: Sized>(
    ddc: &mut UsbDeviceDriverContext,
    request: u8,
    value: u16,
    index: u16,
    buf: Pin<&mut [T]>,
) -> Result<()> {
    // https://github.com/lwhsu/if_axge-kmod/blob/be7510b1bc7e5974963fe17e67462d3689e80631/if_axge.c#L201
    ddc.push_trb_to_ctrl_ep(
        SetupStageTrb::new_vendor_device_in(request, value, index, buf.len() as u16).into(),
    )?;
    ddc.push_trb_to_ctrl_ep(DataStageTrb::new_in(buf).into())?;
    ddc.push_trb_to_ctrl_ep(StatusStageTrb::new_out().into())?;
    ddc.notify_ctrl_ep()?;
    ddc.wait_transfer_event().await
}

async fn write_to_device<T: Sized>(
    ddc: &mut UsbDeviceDriverContext,
    request: u8,
    index: u16,
    reg: u16,
    buf: Pin<&mut [T]>,
) -> Result<()> {
    ddc.push_trb_to_ctrl_ep(
        SetupStageTrb::new_vendor_device_out(request, reg, index, buf.len() as u16).into(),
    )?;
    ddc.push_trb_to_ctrl_ep(DataStageTrb::new_out(buf).into())?;
    ddc.push_trb_to_ctrl_ep(StatusStageTrb::new_in().into())?;
    ddc.notify_ctrl_ep()?;
    ddc.wait_transfer_event().await
}

async fn write_phy_reg(ddc: &mut UsbDeviceDriverContext, index: u16, value: u16) -> Result<()> {
    let mut data = Box::pin([value as u8, (value >> 8) as u8]);
    write_to_device(ddc, REQUEST_ACCESS_PHY, 2, index, data.as_mut()).await
}

async fn read_phy_reg(ddc: &mut UsbDeviceDriverContext, index: u16) -> Result<u16> {
    let mut data = Box::pin([0, 0]);
    read_from_device(ddc, REQUEST_ACCESS_PHY, 2, index, data.as_mut()).await?;
    let data = data.as_ref();
    Ok((data[0] as u16) | (data[1] as u16) << 8)
}

async fn read_mac_reg_u8(ddc: &mut UsbDeviceDriverContext, index: u16) -> Result<u8> {
    let mut data = Box::pin([0]);
    read_from_device(ddc, REQUEST_ACCESS_MAC, 1, index, data.as_mut()).await?;
    let data = data.as_ref();
    Ok(data[0])
}

async fn read_mac_reg_u16(ddc: &mut UsbDeviceDriverContext, index: u16) -> Result<u16> {
    let mut data = Box::pin([0, 0]);
    read_from_device(ddc, REQUEST_ACCESS_MAC, 2, index, data.as_mut()).await?;
    let data = data.as_ref();
    Ok((data[0] as u16) | (data[1] as u16) << 8)
}

async fn write_mac_reg_u16(ddc: &mut UsbDeviceDriverContext, index: u16, value: u16) -> Result<()> {
    let mut data = Box::pin([value as u8, (value >> 8) as u8]);
    write_to_device(ddc, REQUEST_ACCESS_MAC, 2, index, data.as_mut()).await
}

async fn write_mac_reg_u8(ddc: &mut UsbDeviceDriverContext, index: u16, value: u8) -> Result<()> {
    let mut data = Box::pin([value]);
    write_to_device(ddc, REQUEST_ACCESS_MAC, 1, index, data.as_mut()).await
}

async fn request_read_mac_addr(ddc: &mut UsbDeviceDriverContext) -> Result<[u8; 6]> {
    // https://github.com/lwhsu/if_axge-kmod/blob/8afe945e769c87f2eaaf62468e30fe860108d26f/if_axge.c#L414
    let mac = [0u8; 6];
    let mut mac = Box::pin(mac);
    read_from_device(ddc, REQUEST_ACCESS_MAC, MAC_REG_NODE_ID, 6, mac.as_mut()).await?;
    Ok(*mac)
}
async fn reset_phy(ddc: &mut UsbDeviceDriverContext) -> Result<()> {
    // https://github.com/lwhsu/if_axge-kmod/blob/be7510b1bc7e5974963fe17e67462d3689e80631/if_axge.c#L367
    // https://github.com/lwhsu/if_axge-kmod/blob/be7510b1bc7e5974963fe17e67462d3689e80631/if_axge.c#L793
    println!("AX88179: reseting phy...");
    ddc.set_config(1).await?;
    TimeoutFuture::new_ms(10).await;
    write_mac_reg_u16(ddc, MAC_REG_PHYPWR_RSTCTL, 0).await?;
    write_mac_reg_u16(ddc, MAC_REG_PHYPWR_RSTCTL, MAC_REG_PHYPWR_RSTCTL_IPRL).await?;
    TimeoutFuture::new_ms(250).await;
    write_mac_reg_u8(
        ddc,
        MAC_REG_CLK_SELECT,
        MAC_REG_CLK_SELECT_ACS | MAC_REG_CLK_SELECT_BCS,
    )
    .await?;
    TimeoutFuture::new_ms(100).await;
    Ok(())
}
async fn mii_restart_autoneg(ddc: &mut UsbDeviceDriverContext) -> Result<()> {
    let bmcr = read_phy_reg(ddc, MII_BMCR).await?;
    if bmcr & BMCR_ANENABLE != 0 {
        println!("Auto negotiation is supported. enabling...");
        write_phy_reg(ddc, MII_BMCR, bmcr | BMCR_ANRESTART).await
    } else {
        Err(Error::Failed("No auto negotiation support"))
    }
}
async fn init(ddc: &mut UsbDeviceDriverContext) -> Result<()> {
    ddc.set_config(1).await?;
    reset_phy(ddc).await?;

    let mac = request_read_mac_addr(ddc).await?;
    println!(
        "MAC Addr: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac.as_ref()[0],
        mac.as_ref()[1],
        mac.as_ref()[2],
        mac.as_ref()[3],
        mac.as_ref()[4],
        mac.as_ref()[5],
    );

    // Without this, StallError will happen
    // https://github.com/KunYi/hardware_asix_usbnet/blob/80e2c5e18f3e453b701b2369eb6d3d10f31bc0cd/ax88179_178a.c#L1098-L1110
    let mut bulk_in_queue_config = Box::<[u8; 5]>::pin([7, 0x4f, 0x00, 0x24, 0xff]);
    write_to_device(
        ddc,
        REQUEST_ACCESS_MAC,
        5,
        MAC_REG_RX_BULK_IN_QUEUE_CFG,
        bulk_in_queue_config.as_mut(),
    )
    .await?;

    write_mac_reg_u8(ddc, MAC_REG_PAUSE_WATERLVL_LO, 0x34).await?;
    write_mac_reg_u8(ddc, MAC_REG_PAUSE_WATERLVL_HI, 0x52).await?;

    let offloading_protocols = AX_CSUM_OFFLOADING_IP
        | AX_CSUM_OFFLOADING_TCP
        | AX_CSUM_OFFLOADING_UDP
        | AX_CSUM_OFFLOADING_TCPV6
        | AX_CSUM_OFFLOADING_UDPV6;
    write_mac_reg_u8(ddc, MAC_REG_RX_CSUM_OFFLOADING_CTL, offloading_protocols).await?;
    write_mac_reg_u8(ddc, MAC_REG_TX_CSUM_OFFLOADING_CTL, offloading_protocols).await?;

    write_mac_reg_u16(
        ddc,
        MAC_REG_RX_CTL,
        MAC_REG_RX_CTL_PROMISCUOUS
            | MAC_REG_RX_CTL_ACCEPT_ALL_MULTICAST
            | MAC_REG_RX_CTL_ACCEPT_BROADCAST
            | MAC_REG_RX_CTL_ACCEPT_MULTICAST
            | MAC_REG_RX_CTL_ACCEPT_PHYS_ADDR_FROM_MULTICAST_ARRAY
            | MAC_REG_RX_CTL_START
            | MAC_REG_RX_CTL_DROP_CRC_ERROR
            | MAC_REG_RX_CTL_ALIGN_IP_HEADER,
    )
    .await?;

    write_mac_reg_u8(
        ddc,
        MAC_REG_MONITOR_MODE,
        MAC_REG_MONITOR_MODE_PMETYPE | MAC_REG_MONITOR_MODE_PMEPOL | MAC_REG_MONITOR_MODE_RWMP,
    )
    .await?;

    write_mac_reg_u16(
        ddc,
        MAC_REG_MEDIUM_STATUS_MODE,
        MAC_REG_MEDIUM_RECEIVE_EN
            | MAC_REG_MEDIUM_GIGAMODE
            | MAC_REG_MEDIUM_FULL_DUPLEX
            | MAC_REG_MEDIUM_RXFLOW_CTRLEN
            | MAC_REG_MEDIUM_TXFLOW_CTRLEN,
    )
    .await?;

    mii_restart_autoneg(ddc).await?;

    TimeoutFuture::new_ms(1000).await;

    let rx_ctrl = read_mac_reg_u16(ddc, MAC_REG_RX_CTL).await?;
    println!("rx_ctrl: {:#06X}", rx_ctrl);
    let phys_link = read_mac_reg_u8(ddc, MAC_REG_PHYS_LINK_STATUS).await?;
    println!("phys_link: {:#04X}", phys_link);
    let bmsr = read_phy_reg(ddc, MII_BMSR).await?;
    println!("bmsr: {:#04X}", bmsr);
    let gmii_phy_status = read_phy_reg(ddc, GMII_PHY_PHYSR).await?;
    println!("gmii_phys_link: {:#04X}", gmii_phy_status);
    Ok(())
}
pub async fn attach_usb_device(mut ddc: UsbDeviceDriverContext) -> Result<()> {
    init(&mut ddc).await?;
    for ep_desc in ddc.ep_desc_list() {
        let dci = ep_desc.dci();
        let tring = ddc.ep_ring(dci)?.as_ref().expect("tring not found");
        match EndpointType::from(ep_desc) {
            EndpointType::InterruptIn => {
                tring.fill_ring()?;
                ddc.xhci().notify_ep(ddc.slot(), dci)?;
            }
            EndpointType::BulkIn => {
                tring.fill_ring()?;
                ddc.xhci().notify_ep(ddc.slot(), dci)?;
            }
            _ => {}
        }
        println!("dci {}: {:?}", dci, tring);
    }
    let portsc = ddc
        .xhci()
        .portsc(ddc.port())?
        .upgrade()
        .ok_or("PORTSC was invalid")?;
    loop {
        let event_trb = EventFuture::new_command_completion_on_slot(
            ddc.xhci().primary_event_ring(),
            ddc.slot(),
        )
        .await;
        match event_trb {
            Ok(trb) => {
                println!(
                    "slot = {}, dci = {}, length = {}",
                    ddc.slot(),
                    trb.dci(),
                    trb.transfer_length()
                );
                if trb.dci() != 1 {
                    if let Some(ref mut tring) = ddc.ep_ring(trb.dci())?.as_ref() {
                        let cmd = tring.current();
                        let data = unsafe {
                            Mmio::<[u8; 8]>::from_raw(*(cmd.data() as *const usize) as *mut [u8; 8])
                        };
                        hexdump(data.as_ref());
                        let transfer_trb_ptr = trb.data() as usize;
                        tring.dequeue_trb(transfer_trb_ptr)?;
                        println!("{:#018X}", transfer_trb_ptr);
                        ddc.xhci().notify_ep(ddc.slot(), trb.dci())?;
                        println!("{:?}", tring)
                    }
                }
                let status = read_mac_reg_u16(&mut ddc, MAC_REG_MEDIUM_STATUS_MODE).await?;
                println!("status: {:#06X}", status);
                let gmii_phy_status = read_phy_reg(&mut ddc, GMII_PHY_PHYSR).await?;
                println!("gmii_phys_link: {:#04X}", gmii_phy_status);
            }
            Err(e) => {
                println!("Error: {:?}", e);
            }
        }
        if !portsc.ccs() {
            return Err(Error::FailedString(format!(
                "port {} disconnected",
                ddc.port()
            )));
        }
    }
}
