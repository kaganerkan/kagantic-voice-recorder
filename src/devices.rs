//! Selectable capture endpoints, with backend identities separate from labels.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait};

/// List inputs as `(exact CPAL name, friendly label)` pairs.
/// Pass the first element, never the label, to `audio::start_capture_named`.
/// On Linux, aliases of one hardware capture endpoint are collapsed. Default
/// and sound-server routing remain available through the GUI's default option.
pub fn input_devices() -> Result<Vec<(String, String)>> {
    let host = cpal::default_host();
    let mut names = host
        .input_devices()
        .context("enumerate audio input devices")?
        .map(|device| device.name().context("read audio input device name"))
        .collect::<Result<Vec<_>>>()?;
    if names.iter().any(String::is_empty) {
        bail!("an audio input device has an empty backend name and cannot be selected by name");
    }
    names.sort_unstable();
    names.dedup();
    #[cfg(target_os = "linux")]
    let mut devices = linux::capture_endpoints(names);
    #[cfg(not(target_os = "linux"))]
    let mut devices = names
        .into_iter()
        .map(|id| (id.clone(), id))
        .collect::<Vec<_>>();
    disambiguate(&mut devices);
    devices.sort_unstable_by(|a, b| a.1.cmp(&b.1));
    Ok(devices)
}

fn disambiguate(devices: &mut [(String, String)]) {
    let mut counts = HashMap::<&str, usize>::new();
    for (_, label) in devices.iter() {
        *counts.entry(label.as_str()).or_default() += 1;
    }
    let duplicates = devices
        .iter()
        .map(|(_, label)| counts[label.as_str()] > 1)
        .collect::<Vec<_>>();
    drop(counts);
    for ((id, label), duplicate) in devices.iter_mut().zip(duplicates) {
        if duplicate {
            label.push_str(" [");
            label.push_str(id);
            label.push(']');
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use alsa::{pcm::PCM, Direction};
    use std::collections::BTreeMap;

    struct CaptureIdentity {
        card: i32,
        device: u32,
        subdevice: u32,
        label: String,
    }

    pub(super) fn capture_endpoints(names: Vec<String>) -> Vec<(String, String)> {
        select_endpoints(names, capture_identity)
    }

    fn select_endpoints(
        mut names: Vec<String>,
        mut identify: impl FnMut(&str) -> Option<CaptureIdentity>,
    ) -> Vec<(String, String)> {
        // Prefer a direct, unmodified input route over channel-conversion
        // aliases. The retained ID is still one CPAL actually enumerated.
        names.sort_unstable_by(|a, b| route_rank(a).cmp(&route_rank(b)).then_with(|| a.cmp(b)));
        let mut endpoints = BTreeMap::new();
        for id in names {
            if matches!(id.as_str(), "default" | "pulse" | "pipewire") {
                continue;
            }
            let Some(identity) = identify(&id) else {
                continue;
            };
            // Generic conversion/null/server plugins do not identify a
            // physical capture card. They are not additional microphones.
            if identity.card < 0 {
                continue;
            }
            let key = (identity.card, identity.device, identity.subdevice);
            if endpoints.contains_key(&key) {
                continue;
            }
            endpoints.insert(key, (id, identity.label));
        }
        endpoints.into_values().collect()
    }

    fn capture_identity(id: &str) -> Option<CaptureIdentity> {
        let info = match PCM::new(id, Direction::Capture, true).and_then(|pcm| pcm.info()) {
            Ok(info) => info,
            Err(error) => {
                tracing::debug!(device = id, %error, "Capture endpoint identity unavailable");
                return None;
            }
        };
        let card = info.get_card();
        if card < 0 {
            return None;
        }
        let pcm = info.get_name().unwrap_or("Audio input").trim();
        let card_name = alsa::Card::new(card)
            .get_name()
            .ok()
            .filter(|name| !name.trim().is_empty());
        Some(CaptureIdentity {
            card,
            device: info.get_device(),
            subdevice: info.get_subdevice(),
            label: capture_label(card_name, pcm),
        })
    }

    fn capture_label(card_name: Option<String>, pcm: &str) -> String {
        match card_name {
            Some(name) if pcm == name || pcm == "USB Audio" => name,
            Some(name) => format!("{name} — {pcm}"),
            None => format!("{pcm} (ALSA input)"),
        }
    }

    fn route_rank(id: &str) -> u8 {
        if id.starts_with("plughw:") {
            0
        } else if id.starts_with("hw:") {
            1
        } else if id.starts_with("front:") {
            2
        } else {
            3
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn names(ids: &[&str]) -> Vec<String> {
            ids.iter().map(|id| (*id).into()).collect()
        }

        fn identity(card: i32, device: u32, subdevice: u32) -> CaptureIdentity {
            CaptureIdentity {
                card,
                device,
                subdevice,
                label: format!("Capture {card}/{device}/{subdevice}"),
            }
        }

        #[test]
        fn aliases_collapse_to_the_preferred_capture_route_regardless_of_order() {
            let routes = [
                "surround51:CARD=Mic,DEV=0",
                "front:CARD=Mic,DEV=0",
                "hw:CARD=Mic,DEV=0",
                "plughw:CARD=Mic,DEV=0",
            ];
            // Each successive route should supersede the conversion aliases.
            for end in 1..=routes.len() {
                let expected = vec![(routes[end - 1].into(), "Capture 2/0/0".into())];
                for ids in [
                    names(&routes[..end]),
                    names(&routes[..end].iter().rev().copied().collect::<Vec<_>>()),
                ] {
                    assert_eq!(select_endpoints(ids, |_| Some(identity(2, 0, 0))), expected);
                }
            }
        }

        #[test]
        fn separate_cards_devices_and_subdevices_remain_selectable() {
            let selected =
                select_endpoints(names(&["mic-a", "mic-b", "line-in", "sub-input"]), |id| {
                    Some(match id {
                        "mic-a" => identity(0, 0, 0),
                        "mic-b" => identity(1, 0, 0),
                        "line-in" => identity(0, 1, 0),
                        "sub-input" => identity(0, 0, 1),
                        _ => unreachable!(),
                    })
                });
            assert_eq!(
                selected,
                vec![
                    ("mic-a".into(), "Capture 0/0/0".into()),
                    ("sub-input".into(), "Capture 0/0/1".into()),
                    ("line-in".into(), "Capture 0/1/0".into()),
                    ("mic-b".into(), "Capture 1/0/0".into()),
                ]
            );
        }

        #[test]
        fn server_routes_virtual_plugins_and_unavailable_inputs_are_excluded() {
            let selected = select_endpoints(
                names(&[
                    "default",
                    "pulse",
                    "pipewire",
                    "upmix",
                    "null",
                    "unavailable",
                    "microphone",
                ]),
                |id| match id {
                    "unavailable" => None,
                    "upmix" | "null" => Some(identity(-1, 0, 0)),
                    // Even a default route mapped to hardware is not a second
                    // microphone; the GUI already provides its own default.
                    _ => Some(identity(4, 0, 0)),
                },
            );
            assert_eq!(
                selected,
                vec![("microphone".into(), "Capture 4/0/0".into())]
            );
        }

        #[test]
        fn unavailable_preferred_route_does_not_hide_a_working_alias() {
            let selected = select_endpoints(names(&["hw:2", "front:CARD=Mic,DEV=0"]), |id| {
                if id == "hw:2" {
                    None
                } else {
                    Some(identity(2, 0, 0))
                }
            });
            assert_eq!(
                selected,
                vec![("front:CARD=Mic,DEV=0".into(), "Capture 2/0/0".into())]
            );
        }

        #[test]
        fn numeric_alias_identity_is_not_inferred_from_its_name() {
            let selected = select_endpoints(names(&["99", "front:CARD=Mic,DEV=0"]), |_| {
                Some(identity(3, 0, 0))
            });
            assert_eq!(
                selected,
                vec![("front:CARD=Mic,DEV=0".into(), "Capture 3/0/0".into())]
            );
        }

        #[test]
        fn usb_and_repeated_endpoint_names_do_not_clutter_card_labels() {
            assert_eq!(
                capture_label(Some("Webcam microphone".into()), "USB Audio"),
                "Webcam microphone"
            );
            assert_eq!(capture_label(Some("Headset".into()), "Headset"), "Headset");
        }

        #[test]
        fn distinct_endpoint_labels_and_missing_card_metadata_remain_readable() {
            assert_eq!(
                capture_label(Some("Sound card".into()), "Analog input"),
                "Sound card — Analog input"
            );
            assert_eq!(
                capture_label(Some("Sound card".into()), "Digital input"),
                "Sound card — Digital input"
            );
            assert_eq!(capture_label(None, "Microphone"), "Microphone (ALSA input)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_labels_are_disambiguated_without_changing_selection_ids() {
        let mut devices = vec![
            ("input-a".into(), "USB microphone".into()),
            ("input-b".into(), "USB microphone".into()),
            ("line-in".into(), "Line input".into()),
        ];
        disambiguate(&mut devices);
        assert_eq!(
            devices,
            vec![
                ("input-a".into(), "USB microphone [input-a]".into()),
                ("input-b".into(), "USB microphone [input-b]".into()),
                ("line-in".into(), "Line input".into()),
            ]
        );
    }
}
