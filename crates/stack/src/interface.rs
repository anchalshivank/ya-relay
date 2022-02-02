use managed::{ManagedMap, ManagedSlice};
use std::collections::BTreeMap;

use smoltcp::iface::{Interface, InterfaceBuilder, NeighborCache, Route, Routes};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};

use crate::device::CaptureDevice;

pub type CaptureInterface<'a> = Interface<'a, CaptureDevice>;

/// Creates a default network interface
pub fn default_iface<'a>(mac: HardwareAddress) -> CaptureInterface<'a> {
    let neighbor_cache = NeighborCache::new(BTreeMap::new());
    let routes = Routes::new(BTreeMap::new());
    let addrs = Vec::new();

    InterfaceBuilder::new(CaptureDevice::default(), vec![])
        .hardware_addr(mac)
        .neighbor_cache(neighbor_cache)
        .ip_addrs(addrs)
        .routes(routes)
        .finalize()
}

/// Assigns a new interface IP address
pub fn add_iface_address(iface: &mut CaptureInterface, node_ip: IpCidr) {
    iface.update_ip_addrs(|addrs| match addrs {
        ManagedSlice::Owned(ref mut vec) => vec.push(node_ip),
        ManagedSlice::Borrowed(ref slice) => {
            let mut vec = slice.to_vec();
            vec.push(node_ip);
            *addrs = vec.into();
        }
    });
}

/// Adds a new IP route
pub fn add_iface_route(iface: &mut CaptureInterface, net_ip: IpCidr, route: Route) {
    iface.routes_mut().update(|routes| match routes {
        ManagedMap::Owned(ref mut map) => {
            map.insert(net_ip, route);
        }
        ManagedMap::Borrowed(ref map) => {
            let mut map: BTreeMap<IpCidr, Route> = map.iter().filter_map(|e| *e).collect();
            map.insert(net_ip, route);
            *routes = map.into();
        }
    });
}

pub fn to_mac(mac: &[u8]) -> HardwareAddress {
    let mut ethernet = if mac.len() == 6 {
        EthernetAddress::from_bytes(mac)
    } else {
        EthernetAddress::from_bytes(&rand::random::<[u8; 6]>())
    };

    if !ethernet.is_unicast() {
        ethernet.0[0] &= !0x01;
    }

    HardwareAddress::Ethernet(ethernet)
}

pub fn ip_to_mac(ip: IpAddress) -> HardwareAddress {
    match ip {
        IpAddress::Ipv4(ip) => to_mac(ip.as_bytes()),
        IpAddress::Ipv6(ip) => to_mac(ip.as_bytes()),
        _ => to_mac(&rand::random::<[u8; 6]>()),
    }
}
