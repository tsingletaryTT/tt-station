//! Detect Tenstorrent cards attached to THIS machine, without a driver, on macOS.
//!
//! On a Mac a Tenstorrent card is reached through a Thunderbolt/USB4 PCIe tunnel (an eGPU-style
//! enclosure). No driver needs to be loaded for macOS to enumerate it: the IORegistry already
//! carries its IDs, link and BAR layout, and any process may read the IORegistry. So this module
//! answers "what card is plugged in, and what device config does that make this machine?" with
//! plain `ioreg`, no entitlement and no DriverKit extension. That is enough for
//! `tt-station local` to ask the official `tt` CLI which models are right-sized for the card
//! (`tt model list --hw <mesh>`), even before the dext in `macos/TTStationDriver/` can load.
//!
//! Layout:
//! * [`parse_ioreg`] is pure: `ioreg -a` XML in, [`LocalCard`]s out (tested against a real capture,
//!   `fixtures/ioreg-p100a-thunderbolt.xml`, taken from a P100A in a Razer Core X V2 on 2026-09-28);
//! * [`scan`] runs `ioreg` (macOS only) and feeds it to `parse_ioreg`;
//! * [`local_mesh`] turns the cards into one device-mesh label via the shared
//!   [`crate::device_mesh::mesh_for`] table, which the box agent uses too.

use anyhow::{anyhow, Context, Result};
use plist::Value;
use serde::Serialize;

use crate::device_mesh::mesh_for;

/// Tenstorrent's PCI vendor ID (tt-kmd `enumerate.h`).
pub const TT_VENDOR_ID: u16 = 0x1e52;

/// The IORegistry name tt-station's own DriverKit extension registers under once it has claimed a
/// card (`TTBH_SERVICE_NAME` in `macos/TTStationDriver/Shared/TTBlackholeABI.h`). Keep in step.
pub const TT_STATION_DRIVER_NAME: &str = "ttstation-blackhole";

/// One Tenstorrent PCI function (= one ASIC) as macOS sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalCard {
    /// IORegistry entry name, e.g. `pci1e52,b140`.
    pub name: String,
    /// `bus:device:function` from the IORegistry's `pcidebug` property, when present.
    pub location: Option<String>,
    pub vendor_id: u16,
    pub device_id: u16,
    pub subsystem_vendor_id: u16,
    pub subsystem_id: u16,
    /// `grayskull` / `wormhole` / `blackhole`, from the PCI device ID; `None` if unrecognised.
    pub chip: Option<&'static str>,
    /// Board type (`p100a`, `p150c`, …) inferred from the subsystem ID; see [`board_type_for`].
    pub board_type: Option<&'static str>,
    /// Reached through a Thunderbolt/USB4 PCIe tunnel.
    pub tunnelled: bool,
    /// Negotiated PCIe link, decoded from `IOPCIExpressLinkStatus`.
    pub link: Option<PcieLink>,
    /// Lengths in bytes of the device memory ranges macOS assigned, in IORegistry order.
    /// NB: this is NOT indexed by BAR number. Unassigned BARs are simply absent (on the
    /// 2026-09-28 capture BAR4 was never assigned, and the third range is BAR5).
    pub memory_ranges: Vec<u64>,
    /// IORegistry name of the driver that has claimed this card, if any (its first child). `None`
    /// means unclaimed. [`TT_STATION_DRIVER_NAME`] means tt-station's own dext.
    pub driver: Option<String>,
}

impl LocalCard {
    /// Claimed by tt-station's own DriverKit extension, so BARs, NOC reads and telemetry are
    /// available through `TTStationDriver`.
    pub fn has_tt_station_driver(&self) -> bool {
        self.driver.as_deref() == Some(TT_STATION_DRIVER_NAME)
    }
}

/// Negotiated PCIe link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PcieLink {
    /// PCIe generation (1 = 2.5 GT/s … 4 = 16 GT/s, 5 = 32 GT/s).
    pub gen: u8,
    /// Lane count.
    pub width: u8,
}

/// Chip family from the PCI device ID (tt-kmd `enumerate.h`).
pub fn chip_for(device_id: u16) -> Option<&'static str> {
    match device_id {
        0xfaca => Some("grayskull"),
        0x401e => Some("wormhole"),
        0xb140 => Some("blackhole"),
        _ => None,
    }
}

/// Board type from a Blackhole card's PCI subsystem ID.
///
/// The codes are luwen's board-type table (`crates/luwen-api/src/chip/mod.rs`), which luwen
/// applies to the board ID read over the chip's ARC firmware. Treating the PCI *subsystem* ID as
/// the same code is an INFERENCE, confirmed so far for exactly one board: subsystem `0x43`
/// enumerated on a card whose shroud reads "p100a". Limited to Blackhole codes deliberately:
/// the Wormhole rows have not been checked against a subsystem ID at all.
pub fn board_type_for(chip: Option<&str>, subsystem_id: u16) -> Option<&'static str> {
    if chip != Some("blackhole") {
        return None;
    }
    match subsystem_id {
        0x36 => Some("p100"),
        0x40 => Some("p150a"),
        0x41 => Some("p150b"),
        0x42 => Some("p150c"),
        0x43 => Some("p100a"),
        0x44 => Some("p300b"),
        0x45 => Some("p300a"),
        0x46 => Some("p300c"),
        0x47 => Some("galaxy-blackhole"),
        _ => None,
    }
}

/// Decode `IOPCIExpressLinkStatus` (the PCIe Link Status register): bits 3:0 are the current
/// link speed (1 = Gen1 … 5 = Gen5), bits 9:4 the negotiated width. `None` for speed 0 or
/// width 0, which is how a down link reads.
pub fn decode_link_status(status: u64) -> Option<PcieLink> {
    let gen = (status & 0xf) as u8;
    let width = ((status >> 4) & 0x3f) as u8;
    (gen != 0 && width != 0).then_some(PcieLink { gen, width })
}

/// IORegistry PCI ID properties are 4-byte little-endian `data` blobs; the ID is the low 16 bits.
fn id_prop(dict: &plist::Dictionary, key: &str) -> Option<u16> {
    let bytes = dict.get(key)?.as_data()?;
    (bytes.len() >= 2).then(|| u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// The driver attached to an IOPCIDevice: the name of its first IORegistry child (present with
/// `ioreg -d 2`). A matched dext appears here as an `IOUserService` child.
fn attached_driver(dict: &plist::Dictionary) -> Option<String> {
    dict.get("IORegistryEntryChildren")?
        .as_array()?
        .iter()
        .find_map(|c| c.as_dictionary()?.get("IORegistryEntryName")?.as_string().map(str::to_string))
}

/// Parse `ioreg -a -r -c IOPCIDevice -d 2` output (an XML plist array of IOPCIDevice
/// dictionaries) and return the Tenstorrent ones. Non-TT devices are skipped silently; a TT
/// device missing an ID property is an error (the capture is not what we think it is).
pub fn parse_ioreg(xml: &[u8]) -> Result<Vec<LocalCard>> {
    let root = Value::from_reader_xml(xml).context("ioreg output is not an XML plist")?;
    let entries = root
        .as_array()
        .ok_or_else(|| anyhow!("expected ioreg -a to produce a plist array"))?;

    let mut cards = Vec::new();
    for entry in entries {
        let Some(dict) = entry.as_dictionary() else { continue };
        if id_prop(dict, "vendor-id") != Some(TT_VENDOR_ID) {
            continue;
        }
        let name = dict
            .get("IORegistryEntryName")
            .and_then(Value::as_string)
            .unwrap_or("?")
            .to_string();
        let need = |key: &str| {
            id_prop(dict, key).ok_or_else(|| anyhow!("Tenstorrent device {name} has no {key}"))
        };
        let device_id = need("device-id")?;
        let subsystem_id = need("subsystem-id")?;
        let chip = chip_for(device_id);

        let memory_ranges = dict
            .get("IODeviceMemory")
            .and_then(Value::as_array)
            .map(|ranges| {
                ranges
                    .iter()
                    // Each range is itself an array of {address, length} segments; sum them.
                    .map(|segs| {
                        segs.as_array()
                            .map(|segs| {
                                segs.iter()
                                    .filter_map(|s| s.as_dictionary()?.get("length")?.as_unsigned_integer())
                                    .sum()
                            })
                            .unwrap_or(0)
                    })
                    .collect()
            })
            .unwrap_or_default();

        cards.push(LocalCard {
            location: dict.get("pcidebug").and_then(Value::as_string).map(str::to_string),
            vendor_id: TT_VENDOR_ID,
            device_id,
            subsystem_vendor_id: need("subsystem-vendor-id")?,
            subsystem_id,
            chip,
            board_type: board_type_for(chip, subsystem_id),
            tunnelled: dict.get("IOPCITunnelled").and_then(Value::as_boolean).unwrap_or(false),
            link: dict
                .get("IOPCIExpressLinkStatus")
                .and_then(Value::as_unsigned_integer)
                .and_then(decode_link_status),
            memory_ranges,
            driver: attached_driver(dict),
            name,
        });
    }
    Ok(cards)
}

/// Enumerate the Tenstorrent cards attached to this Mac. Runs `/usr/sbin/ioreg` (read-only,
/// no privileges needed) and parses it. On other platforms this is an error pointing at the
/// official tooling, which owns local detection on Linux (`tt device status` via tt-smi).
pub fn scan() -> Result<Vec<LocalCard>> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/sbin/ioreg")
            // Depth 2 so each device carries its children, i.e. the driver (if any) that claimed it.
            .args(["-a", "-r", "-c", "IOPCIDevice", "-d", "2"])
            .output()
            .context("running /usr/sbin/ioreg")?;
        if !out.status.success() {
            return Err(anyhow!("ioreg exited with {}", out.status));
        }
        parse_ioreg(&out.stdout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(anyhow!(
            "local detection via the IORegistry is macOS-only; on Linux use `tt device status`"
        ))
    }
}

/// The device-mesh label for this machine's cards, or `None` when there are none, they are a
/// mixed set, a board type is unknown, or the (type, count) has no confirmed mesh. Same
/// "don't guess" contract as the agent.
pub fn local_mesh(cards: &[LocalCard]) -> Option<&'static str> {
    let first = cards.first()?.board_type?;
    if !cards.iter().all(|c| c.board_type == Some(first)) {
        return None;
    }
    mesh_for(first, cards.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("fixtures/ioreg-p100a-thunderbolt.xml");

    #[test]
    fn real_capture_finds_exactly_the_p100a() {
        // The fixture also holds the Mac's Broadcom Wi-Fi (14e4:4434); it must be skipped.
        let cards = parse_ioreg(FIXTURE).unwrap();
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(c.name, "pci1e52,b140");
        assert_eq!((c.vendor_id, c.device_id), (0x1e52, 0xb140));
        assert_eq!((c.subsystem_vendor_id, c.subsystem_id), (0x1e52, 0x43));
        assert_eq!(c.chip, Some("blackhole"));
        assert_eq!(c.board_type, Some("p100a"));
        assert!(c.tunnelled);
        assert_eq!(c.link, Some(PcieLink { gen: 4, width: 4 }));
        assert_eq!(c.location.as_deref(), Some("3:0:0"));
        // BAR0 512 MiB, BAR2 1 MiB, BAR5 16 B — BAR4 was never assigned.
        assert_eq!(c.memory_ranges, vec![512 << 20, 1 << 20, 16]);
    }

    #[test]
    fn unclaimed_card_has_no_driver() {
        let c = &parse_ioreg(FIXTURE).unwrap()[0];
        assert_eq!(c.driver, None);
        assert!(!c.has_tt_station_driver());
    }

    #[test]
    fn attached_dext_is_read_from_a_real_capture() {
        // The same capture's Wi-Fi is claimed by Apple's DriverKit dext, the exact shape our own
        // dext will have once it loads (an IOUserService child of the IOPCIDevice).
        let root = Value::from_reader_xml(FIXTURE).unwrap();
        let wlan = root.as_array().unwrap().iter()
            .filter_map(Value::as_dictionary)
            .find(|d| d.get("IORegistryEntryName").and_then(Value::as_string) == Some("wlan"))
            .unwrap();
        assert_eq!(attached_driver(wlan).as_deref(), Some("AppleBCMWLANBusInterfacePCIe"));
    }

    #[test]
    fn our_driver_is_recognised_by_name() {
        let mut c = parse_ioreg(FIXTURE).unwrap().remove(0);
        c.driver = Some(TT_STATION_DRIVER_NAME.into());
        assert!(c.has_tt_station_driver());
        c.driver = Some("TinyGPUDriver".into());
        assert!(!c.has_tt_station_driver());
    }

    #[test]
    fn real_capture_is_a_p100_machine() {
        let cards = parse_ioreg(FIXTURE).unwrap();
        assert_eq!(local_mesh(&cards), Some("p100"));
    }

    #[test]
    fn link_status_decoding() {
        assert_eq!(decode_link_status(0x1044), Some(PcieLink { gen: 4, width: 4 }));
        assert_eq!(decode_link_status(0x1083), Some(PcieLink { gen: 3, width: 8 }));
        assert_eq!(decode_link_status(0xffff_0000), None); // speed 0: link down
    }

    #[test]
    fn board_type_only_inferred_for_blackhole() {
        assert_eq!(board_type_for(Some("blackhole"), 0x43), Some("p100a"));
        assert_eq!(board_type_for(Some("wormhole"), 0x43), None);
        assert_eq!(board_type_for(Some("blackhole"), 0x99), None);
    }

    #[test]
    fn no_cards_or_mixed_cards_have_no_mesh() {
        assert_eq!(local_mesh(&[]), None);
        let mut a = parse_ioreg(FIXTURE).unwrap().remove(0);
        let mut b = a.clone();
        b.board_type = Some("p150c");
        assert_eq!(local_mesh(&[a.clone(), b]), None);
        a.board_type = None;
        assert_eq!(local_mesh(&[a]), None);
    }

    #[test]
    fn garbage_is_an_error_not_an_empty_list() {
        assert!(parse_ioreg(b"not a plist").is_err());
    }
}
