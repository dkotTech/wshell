//! NetworkManager over D-Bus for the `system.network` permission. Only the supervisor talks to
//! NetworkManager: the worker asks over IPC, and `hostsvc` checks the mode first.

use std::collections::HashMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};
use zbus::proxy::{CacheProperties, Defaults};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::ipc::{AccessPoint, NetDevice};

const NONE: ObjectPath<'static> = ObjectPath::from_static_str_unchecked("/");

/// `NM_DEVICE_TYPE_WIFI`.
const WIFI: u32 = 2;
/// `NM_802_11_AP_FLAGS_PRIVACY`: WEP or better.
const AP_PRIVACY: u32 = 0x1;
/// `NM_802_11_AP_SEC_KEY_MGMT_PSK` and `_SAE` in the WPA and RSN flags.
const KEY_MGMT_PSK: u32 = 0x100;
const KEY_MGMT_SAE: u32 = 0x400;

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait Manager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn get_device_by_ip_iface(&self, iface: &str) -> zbus::Result<OwnedObjectPath>;
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn add_and_activate_connection(
        &self,
        connection: HashMap<&str, HashMap<&str, Value<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath)>;
}

#[zbus::proxy(interface = "org.freedesktop.NetworkManager.Device", default_service = "org.freedesktop.NetworkManager")]
trait Device {
    fn disconnect(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn hw_address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip6_config(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
    /// The saved profiles that can be activated on this device.
    #[zbus(property)]
    fn available_connections(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Wireless {
    fn get_all_access_points(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn request_scan(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
}

#[zbus::proxy(interface = "org.freedesktop.NetworkManager.AccessPoint", default_service = "org.freedesktop.NetworkManager")]
trait AccessPoint {
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn strength(&self) -> zbus::Result<u8>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn wpa_flags(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn rsn_flags(&self) -> zbus::Result<u32>;
}

#[zbus::proxy(interface = "org.freedesktop.NetworkManager.IP4Config", default_service = "org.freedesktop.NetworkManager")]
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
    /// In network byte order.
    #[zbus(property)]
    fn nameservers(&self) -> zbus::Result<Vec<u32>>;
}

#[zbus::proxy(interface = "org.freedesktop.NetworkManager.IP6Config", default_service = "org.freedesktop.NetworkManager")]
trait Ip6Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedValue>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn nameservers(&self) -> zbus::Result<Vec<Vec<u8>>>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Active {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
    interface = "org.freedesktop.NetworkManager.Settings.Connection",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Profile {
    /// Without secrets.
    fn get_settings(&self) -> zbus::Result<HashMap<String, HashMap<String, OwnedValue>>>;
}

pub async fn connect() -> Result<zbus::Connection> {
    zbus::Connection::system().await.context("D-Bus system bus")
}

/// A proxy for one object. Properties are read once per call, so nothing is cached and no
/// signals are subscribed to.
async fn at<P>(conn: &zbus::Connection, path: &OwnedObjectPath) -> Result<P>
where
    P: From<zbus::Proxy<'static>> + Defaults,
{
    Ok(zbus::proxy::Builder::<P>::new(conn)
        .path(path.clone())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?)
}

pub async fn devices(conn: &zbus::Connection) -> Result<Vec<NetDevice>> {
    let paths = ManagerProxy::new(conn).await?.get_devices().await.map_err(explain)?;
    let mut devices = Vec::with_capacity(paths.len());
    for path in &paths {
        devices.push(device(conn, path).await?);
    }
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(devices)
}

async fn device(conn: &zbus::Connection, path: &OwnedObjectPath) -> Result<NetDevice> {
    let d: DeviceProxy = at(conn, path).await?;
    let mut dev = NetDevice {
        name: d.interface().await?,
        kind: kind(d.device_type().await?).to_string(),
        state: state(d.state().await?).to_string(),
        connection: None,
        hw_address: d.hw_address().await?,
        ipv4: Vec::new(),
        ipv6: Vec::new(),
        gateway: None,
        dns: Vec::new(),
    };
    let active = d.active_connection().await?;
    if *active != NONE {
        dev.connection = Some(at::<ActiveProxy>(conn, &active).await?.id().await?);
    }
    let ip4 = d.ip4_config().await?;
    if *ip4 != NONE {
        let c: Ip4ConfigProxy = at(conn, &ip4).await?;
        dev.ipv4 = addresses(c.address_data().await?);
        dev.gateway = Some(c.gateway().await?).filter(|g| !g.is_empty());
        dev.dns.extend(c.nameservers().await?.into_iter().map(|n| Ipv4Addr::from(n.to_ne_bytes()).to_string()));
    }
    let ip6 = d.ip6_config().await?;
    if *ip6 != NONE {
        let c: Ip6ConfigProxy = at(conn, &ip6).await?;
        dev.ipv6 = addresses(c.address_data().await?);
        if dev.gateway.is_none() {
            dev.gateway = Some(c.gateway().await?).filter(|g| !g.is_empty());
        }
        for n in c.nameservers().await? {
            if let Ok(octets) = <[u8; 16]>::try_from(n) {
                dev.dns.push(Ipv6Addr::from(octets).to_string());
            }
        }
    }
    Ok(dev)
}

/// `AddressData` entries as `address/prefix`.
fn addresses(data: Vec<HashMap<String, OwnedValue>>) -> Vec<String> {
    data.iter()
        .filter_map(|a| {
            let address = a.get("address")?.downcast_ref::<&str>().ok()?;
            let prefix = a.get("prefix")?.downcast_ref::<u32>().ok()?;
            Some(format!("{address}/{prefix}"))
        })
        .collect()
}

/// `NMDeviceType`, the common ones.
fn kind(t: u32) -> &'static str {
    match t {
        1 => "ethernet",
        2 => "wifi",
        5 => "bluetooth",
        8 => "modem",
        10 => "bond",
        11 => "vlan",
        13 => "bridge",
        16 => "tun",
        20 => "veth",
        22 => "dummy",
        29 => "wireguard",
        30 => "wifi-p2p",
        32 => "loopback",
        _ => "other",
    }
}

/// `NMDeviceState`, the activation steps folded into `connecting`.
fn state(s: u32) -> &'static str {
    match s {
        10 => "unmanaged",
        20 => "unavailable",
        30 => "disconnected",
        40..=90 => "connecting",
        100 => "connected",
        110 => "deactivating",
        120 => "failed",
        _ => "unknown",
    }
}

async fn device_path(conn: &zbus::Connection, name: &str) -> Result<OwnedObjectPath> {
    ManagerProxy::new(conn).await?.get_device_by_ip_iface(name).await.map_err(explain)
}

async fn wifi_device(conn: &zbus::Connection, name: &str) -> Result<(OwnedObjectPath, WirelessProxy<'static>)> {
    let path = device_path(conn, name).await?;
    if at::<DeviceProxy>(conn, &path).await?.device_type().await? != WIFI {
        bail!("{name} is not a Wi-Fi device");
    }
    let wireless = at(conn, &path).await?;
    Ok((path, wireless))
}

/// One access point per network: the strongest.
struct Network {
    path: OwnedObjectPath,
    ap: AccessPoint,
    wpa: u32,
    rsn: u32,
}

async fn networks(conn: &zbus::Connection, wireless: &WirelessProxy<'_>) -> Result<Vec<Network>> {
    let active = wireless.active_access_point().await?;
    let mut networks: Vec<Network> = Vec::new();
    for path in wireless.get_all_access_points().await.map_err(explain)? {
        let p: AccessPointProxy = at(conn, &path).await?;
        let ssid = String::from_utf8_lossy(&p.ssid().await?).into_owned();
        if ssid.is_empty() {
            continue; // a hidden network
        }
        let (wpa, rsn) = (p.wpa_flags().await?, p.rsn_flags().await?);
        let ap = AccessPoint {
            secured: p.flags().await? & AP_PRIVACY != 0 || wpa != 0 || rsn != 0,
            strength: p.strength().await?,
            active: path == active,
            saved: false,
            ssid,
        };
        match networks.iter_mut().find(|n| n.ap.ssid == ap.ssid) {
            Some(n) => {
                n.ap.active |= ap.active;
                if ap.strength > n.ap.strength {
                    let active = n.ap.active;
                    *n = Network { path, ap: AccessPoint { active, ..ap }, wpa, rsn };
                }
            }
            None => networks.push(Network { path, ap, wpa, rsn }),
        }
    }
    networks.sort_by_key(|n| std::cmp::Reverse(n.ap.strength));
    Ok(networks)
}

/// The device's saved Wi-Fi profiles by network name.
async fn saved(conn: &zbus::Connection, device: &OwnedObjectPath) -> Result<HashMap<String, OwnedObjectPath>> {
    let mut saved = HashMap::new();
    for path in at::<DeviceProxy>(conn, device).await?.available_connections().await? {
        let settings = at::<ProfileProxy>(conn, &path).await?.get_settings().await.map_err(explain)?;
        let ssid = settings
            .get("802-11-wireless")
            .and_then(|w| w.get("ssid"))
            .and_then(|s| Vec::<u8>::try_from(s.try_clone().ok()?).ok());
        if let Some(ssid) = ssid {
            saved.insert(String::from_utf8_lossy(&ssid).into_owned(), path);
        }
    }
    Ok(saved)
}

pub async fn access_points(conn: &zbus::Connection, device: &str) -> Result<Vec<AccessPoint>> {
    let (path, wireless) = wifi_device(conn, device).await?;
    let saved = saved(conn, &path).await?;
    Ok(networks(conn, &wireless)
        .await?
        .into_iter()
        .map(|n| AccessPoint { saved: saved.contains_key(&n.ap.ssid), ..n.ap })
        .collect())
}

pub async fn scan(conn: &zbus::Connection, device: &str) -> Result<()> {
    let (_, wireless) = wifi_device(conn, device).await?;
    wireless.request_scan(HashMap::new()).await.map_err(explain)
}

/// With the best saved profile for the device, as NetworkManager picks it.
pub async fn connect_device(conn: &zbus::Connection, device: &str) -> Result<()> {
    let path = device_path(conn, device).await?;
    let m = ManagerProxy::new(conn).await?;
    m.activate_connection(&NONE, &path, &NONE).await.map(drop).map_err(explain)
}

pub async fn disconnect(conn: &zbus::Connection, device: &str) -> Result<()> {
    let path = device_path(conn, device).await?;
    at::<DeviceProxy>(conn, &path).await?.disconnect().await.map_err(explain)
}

/// Without a password, with the saved profile of the network; with one, with a new profile
/// (NetworkManager saves it). Like `nmcli device wifi connect`.
pub async fn connect_wifi(conn: &zbus::Connection, device: &str, ssid: &str, password: Option<&str>) -> Result<()> {
    let (path, wireless) = wifi_device(conn, device).await?;
    let m = ManagerProxy::new(conn).await?;
    if password.is_none()
        && let Some(profile) = saved(conn, &path).await?.remove(ssid)
    {
        return m.activate_connection(&profile, &path, &NONE).await.map(drop).map_err(explain);
    }
    let networks = networks(conn, &wireless).await?;
    let Some(network) = networks.iter().find(|n| n.ap.ssid == ssid) else {
        bail!("network {ssid} is not in range (scan again)");
    };
    let mut settings = HashMap::from([("802-11-wireless", HashMap::from([("ssid", Value::from(ssid.as_bytes()))]))]);
    if network.ap.secured {
        let Some(password) = password else { bail!("network {ssid} needs a password") };
        let mgmt = network.wpa | network.rsn;
        let key_mgmt = if mgmt & KEY_MGMT_PSK != 0 {
            "wpa-psk"
        } else if mgmt & KEY_MGMT_SAE != 0 {
            "sae"
        } else if network.wpa == 0 && network.rsn == 0 {
            bail!("network {ssid} uses WEP, which is not supported")
        } else {
            bail!("network {ssid} uses 802.1X (enterprise) authentication, which is not supported")
        };
        settings.insert(
            "802-11-wireless-security",
            HashMap::from([("key-mgmt", Value::from(key_mgmt)), ("psk", Value::from(password))]),
        );
    }
    m.add_and_activate_connection(settings, &path, &network.path).await.map(drop).map_err(explain)
}

/// NetworkManager's errors as they are, with a hint for the usual one of a system service.
fn explain(e: zbus::Error) -> anyhow::Error {
    match &e {
        zbus::Error::MethodError(name, msg, _)
            if name.as_str().ends_with("PermissionDenied") || name.as_str().ends_with("AccessDenied") =>
        {
            anyhow::anyhow!(
                "NetworkManager denied it: {} (a polkit rule must allow the shelld user)",
                msg.as_deref().unwrap_or("not authorized")
            )
        }
        zbus::Error::MethodError(name, _, _) if name.as_str().ends_with("ServiceUnknown") => {
            anyhow::anyhow!("NetworkManager is not running")
        }
        zbus::Error::MethodError(_, Some(msg), _) => anyhow::anyhow!("NetworkManager: {msg}"),
        _ => anyhow::Error::new(e),
    }
}
