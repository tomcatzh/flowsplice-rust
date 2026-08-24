use std::{
    cmp::Ordering,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::{process::Command, sync::watch, time::sleep};
use tracing::{info, warn};

const UBUS_PATH: &str = "/bin/ubus";
const MAX_STATUS_BYTES: usize = 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkState {
    pub(crate) l3_device: String,
    pub(crate) ipv4_address: Option<Ipv4Addr>,
    pub(crate) ipv6_address: Option<Ipv6Addr>,
}

#[derive(Deserialize)]
struct InterfaceStatus {
    #[serde(default)]
    up: bool,
    l3_device: Option<String>,
    #[serde(default, rename = "ipv4-address")]
    ipv4_addresses: Vec<InterfaceAddress>,
    #[serde(default, rename = "ipv6-address")]
    ipv6_addresses: Vec<InterfaceAddress>,
}

#[derive(Deserialize)]
struct InterfaceAddress {
    address: IpAddr,
    preferred: Option<u64>,
    valid: Option<u64>,
}

impl InterfaceAddress {
    fn is_usable(&self) -> bool {
        let address_is_usable = match self.address {
            IpAddr::V4(address) => {
                !address.is_unspecified()
                    && !address.is_loopback()
                    && !address.is_multicast()
                    && !address.is_link_local()
            }
            IpAddr::V6(address) => {
                !address.is_unspecified()
                    && !address.is_loopback()
                    && !address.is_multicast()
                    && !address.is_unicast_link_local()
            }
        };
        address_is_usable && self.preferred != Some(0) && self.valid != Some(0)
    }

    fn rank(&self) -> (u8, u64, u64) {
        (
            public_rank(self.address),
            self.preferred.unwrap_or(u64::MAX),
            self.valid.unwrap_or(u64::MAX),
        )
    }
}

fn public_rank(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(address) if is_public_ipv4(address) => 1,
        IpAddr::V6(address) if is_global_unicast_ipv6(address) => 1,
        _ => 0,
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    !(address.is_private()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_link_local())
}

fn is_global_unicast_ipv6(address: Ipv6Addr) -> bool {
    address.segments()[0] & 0xe000 == 0x2000
}

pub(crate) fn parse_status(bytes: &[u8]) -> Result<Option<NetworkState>> {
    let status: InterfaceStatus =
        serde_json::from_slice(bytes).context("failed to parse OpenWrt interface status")?;
    if !status.up {
        return Ok(None);
    }
    let Some(l3_device) = status.l3_device.filter(|device| !device.is_empty()) else {
        return Ok(None);
    };
    let ipv4_address = select_address(status.ipv4_addresses).and_then(|address| match address {
        IpAddr::V4(address) => Some(address),
        IpAddr::V6(_) => None,
    });
    let ipv6_address = select_address(status.ipv6_addresses).and_then(|address| match address {
        IpAddr::V4(_) => None,
        IpAddr::V6(address) => Some(address),
    });
    Ok(Some(NetworkState {
        l3_device,
        ipv4_address,
        ipv6_address,
    }))
}

fn select_address(addresses: Vec<InterfaceAddress>) -> Option<IpAddr> {
    addresses
        .into_iter()
        .filter(InterfaceAddress::is_usable)
        .max_by(|left, right| {
            left.rank()
                .cmp(&right.rank())
                .then_with(|| compare_ip(left.address, right.address))
        })
        .map(|address| address.address)
}

fn compare_ip(left: IpAddr, right: IpAddr) -> Ordering {
    match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) => left.octets().cmp(&right.octets()),
        (IpAddr::V6(left), IpAddr::V6(right)) => left.octets().cmp(&right.octets()),
        (IpAddr::V4(_), IpAddr::V6(_)) => Ordering::Less,
        (IpAddr::V6(_), IpAddr::V4(_)) => Ordering::Greater,
    }
}

async fn query_status(network: &str) -> Result<Option<NetworkState>> {
    let object = format!("network.interface.{network}");
    let output = Command::new(UBUS_PATH)
        .args(["call", &object, "status"])
        .output()
        .await
        .with_context(|| format!("failed to execute {UBUS_PATH} for {network}"))?;
    if !output.status.success() {
        bail!(
            "{UBUS_PATH} status query for {network} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if output.stdout.len() > MAX_STATUS_BYTES {
        bail!("OpenWrt interface status for {network} exceeds 1 MiB");
    }
    parse_status(&output.stdout)
}

pub(crate) async fn monitor(
    network: String,
    sender: watch::Sender<Option<NetworkState>>,
) -> Result<()> {
    let mut last = None;
    loop {
        match query_status(&network).await {
            Ok(current) => {
                if current != last {
                    if let Some(state) = &current {
                        info!(
                            openwrt_network = %network,
                            device = %state.l3_device,
                            ipv4_address = ?state.ipv4_address,
                            ipv6_address = ?state.ipv6_address,
                            "OpenWrt endpoint candidates changed"
                        );
                    } else {
                        warn!(
                            openwrt_network = %network,
                            "OpenWrt network is unavailable"
                        );
                    }
                    sender.send_replace(current.clone());
                    last = current;
                }
            }
            Err(error) => {
                warn!(
                    openwrt_network = %network,
                    %error,
                    "failed to refresh OpenWrt endpoint candidates"
                );
                if last.is_some() {
                    sender.send_replace(None);
                    last = None;
                }
            }
        }
        sleep(REFRESH_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_preferred_addresses_and_device() -> Result<()> {
        let status = br#"{
            "up": true,
            "l3_device": "pppoe-wan",
            "ipv4-address": [
                {"address":"192.168.1.2","mask":24},
                {"address":"8.8.8.8","mask":24,"preferred":120,"valid":240},
                {"address":"9.9.9.9","mask":24,"preferred":60,"valid":240}
            ],
            "ipv6-address": [
                {"address":"fe80::1","mask":64,"preferred":4294967295,"valid":4294967295},
                {"address":"2001:db8::2","mask":64,"preferred":0,"valid":120},
                {"address":"240e:1234::10","mask":64,"preferred":120,"valid":240},
                {"address":"240e:1234::20","mask":64,"preferred":60,"valid":240}
            ]
        }"#;
        assert_eq!(
            parse_status(status)?,
            Some(NetworkState {
                l3_device: "pppoe-wan".to_owned(),
                ipv4_address: Some("8.8.8.8".parse()?),
                ipv6_address: Some("240e:1234::10".parse()?),
            })
        );
        Ok(())
    }

    #[test]
    fn keeps_private_and_ula_addresses_for_reachable_lan_relays() -> Result<()> {
        let status = br#"{
            "up":true,
            "l3_device":"br-lan",
            "ipv4-address":[{"address":"192.168.1.1"}],
            "ipv6-address":[{"address":"fd00::1"},{"address":"fe80::1"}]
        }"#;
        assert_eq!(
            parse_status(status)?,
            Some(NetworkState {
                l3_device: "br-lan".to_owned(),
                ipv4_address: Some("192.168.1.1".parse()?),
                ipv6_address: Some("fd00::1".parse()?),
            })
        );
        Ok(())
    }

    #[test]
    fn unavailable_or_addressless_status_is_not_advertised() -> Result<()> {
        assert_eq!(
            parse_status(
                br#"{"up":false,"l3_device":"eth1","ipv4-address":[{"address":"192.0.2.1"}]}"#
            )?,
            None
        );
        assert_eq!(
            parse_status(br#"{"up":true,"ipv6-address":[{"address":"240e::1"}]}"#)?,
            None
        );
        assert_eq!(
            parse_status(
                br#"{"up":true,"l3_device":"eth1","ipv6-address":[{"address":"fe80::1"}]}"#
            )?,
            Some(NetworkState {
                l3_device: "eth1".to_owned(),
                ipv4_address: None,
                ipv6_address: None,
            })
        );
        Ok(())
    }
}
