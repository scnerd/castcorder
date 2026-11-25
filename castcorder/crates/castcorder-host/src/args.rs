//! The host's command-line flags, shared by the `castcorder-host` binary and
//! the desktop app (which adds flags of its own).

use std::net::IpAddr;
use std::path::PathBuf;

use crate::Config;

/// The host flags, for a usage line.
pub const USAGE: &str = "--dist DIR [--library DIR] [--room NAME] [--idle-gap MINUTES] [--bind IP] \
[--port N] [--http-port N] [--udp-port N] [--advertise IP]... [--rendezvous URL|off] [--relay-only] [--bandwidth-cap KBPS] \
[--participate [DEVICE]] [--name NAME] [--keep-wav true|false] [--align resample|offset-only|off] [--aligned-format flac|wav] [--open] \
[--record-video true|false] [--transcribe MODEL|off]";

impl Config {
    /// Apply the host flags in `args` (see [`USAGE`]) to this config, in
    /// order. Arguments that are not host flags are returned, in order, for
    /// the caller to handle; a host flag with a bad value is an error.
    pub fn apply_args(
        &mut self,
        args: impl IntoIterator<Item = String>,
    ) -> Result<Vec<String>, String> {
        let a = self;
        let mut rest = vec![];
        let mut args = args.into_iter().peekable();
        while let Some(flag) = args.next() {
            let mut value = || args.next().ok_or_else(|| format!("{flag} needs a value"));
            let port =
                |v: String| -> Result<u16, String> { v.parse().map_err(|_| "bad port".into()) };
            let ip = |v: String| -> Result<IpAddr, String> {
                v.parse().map_err(|_| "bad IP address".into())
            };
            let flag_bool = |v: String, what: &str| match v.as_str() {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(format!("{what} is true or false")),
            };
            match flag.as_str() {
                "--dist" => a.dist = PathBuf::from(value()?),
                "--library" => a.library = PathBuf::from(value()?),
                // `--call` is the name from before rooms: a room of that name.
                "--room" | "--call" => a.room = value()?,
                "--idle-gap" => {
                    let minutes: f64 = value()?
                        .parse()
                        .ok()
                        .filter(|m: &f64| m.is_finite() && *m >= 0.0)
                        .ok_or_else(|| "--idle-gap is minutes (a number)".to_string())?;
                    a.idle_gap = std::time::Duration::from_secs_f64(minutes * 60.0);
                }
                "--bind" => a.bind = ip(value()?)?,
                "--port" => a.https_port = port(value()?)?,
                "--http-port" => a.http_port = port(value()?)?,
                "--udp-port" => a.udp_port = port(value()?)?,
                "--advertise" => a.advertise.push(ip(value()?)?),
                "--rendezvous" => {
                    let url = value()?;
                    // `off`: no rendezvous, not even the default.
                    if url == "off" {
                        a.rendezvous = None;
                    } else {
                        crate::rendezvous::validate(&url)?;
                        a.rendezvous = Some(url.trim_end_matches('/').to_string());
                    }
                }
                "--relay-only" => a.relay_only = true,
                "--bandwidth-cap" => {
                    a.bandwidth_cap = Some(
                        value()?
                            .parse()
                            .map_err(|_| "bad --bandwidth-cap (kbps)".to_string())?,
                    )
                }
                "--open" => a.open = true,
                "--record-video" => a.record_video = Some(flag_bool(value()?, "--record-video")?),
                "--participate" => {
                    if !crate::NATIVE_CAPTURE {
                        return Err(
                            "--participate needs a build with the native-capture feature (task host)"
                                .into(),
                        );
                    }
                    let device = args.next_if(|v| !v.starts_with("--"));
                    a.participate = Some(device);
                }
                "--name" => {
                    let n: String = value()?
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .take(32)
                        .collect();
                    if n.is_empty() {
                        return Err("--name needs letters, digits or '-'".into());
                    }
                    a.name = n;
                }
                "--align" => {
                    let mode = match value()?.as_str() {
                        "resample" => Some(castcorder_align::Mode::Resample),
                        "offset-only" => Some(castcorder_align::Mode::OffsetOnly),
                        "off" => None,
                        _ => return Err("--align is resample, offset-only or off".into()),
                    };
                    a.align = mode.map(|mode| castcorder_align::AlignConfig {
                        mode,
                        ..a.align.unwrap_or_default()
                    });
                }
                "--keep-wav" => a.keep_wav = flag_bool(value()?, "--keep-wav")?,
                "--transcribe" => {
                    let v = value()?;
                    // `sim`: the port's simulation, which hears only its tone
                    // code (the browser tests).
                    a.transcribe = match v.as_str() {
                        "off" => None,
                        "sim" => Some(crate::transcript::Engine::Simulated),
                        _ => Some(crate::transcript::Engine::Whisper(v.into())),
                    };
                }
                "--aligned-format" => {
                    let format = match value()?.as_str() {
                        "flac" => castcorder_align::OutputFormat::Flac,
                        "wav" => castcorder_align::OutputFormat::Wav,
                        _ => return Err("--aligned-format is flac or wav".into()),
                    };
                    if let Some(c) = &mut a.align {
                        c.format = format;
                    }
                }
                _ => rest.push(flag),
            }
        }
        Ok(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn host_flags_apply_and_the_rest_come_back_in_order() {
        let mut c = Config::new("d");
        let rest = c
            .apply_args(args(
                "--smoke --call show --idle-gap 1.5 --http-port 0 --open x --align off --record-video true",
            ))
            .unwrap();
        assert_eq!(rest, args("--smoke x"));
        assert_eq!(c.room, "show");
        assert_eq!(c.idle_gap, std::time::Duration::from_secs(90));
        let mut r = Config::new("d");
        r.apply_args(args("--room studio")).unwrap();
        assert_eq!(r.room, "studio");
        assert_eq!(c.http_port, 0);
        assert!(c.open);
        assert!(c.align.is_none());
        assert_eq!(c.record_video, Some(true));
    }

    #[test]
    fn bad_values_are_errors() {
        for bad in [
            "--port x",
            "--call",
            "--keep-wav maybe",
            "--name !!",
            "--idle-gap soon",
        ] {
            assert!(Config::new("d").apply_args(args(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn rendezvous_takes_a_url_or_off() {
        let mut c = Config::new("d");
        c.apply_args(args("--rendezvous https://rv.example.org/"))
            .unwrap();
        assert_eq!(c.rendezvous.as_deref(), Some("https://rv.example.org"));
        c.apply_args(args("--rendezvous off")).unwrap();
        assert_eq!(c.rendezvous, None);
        assert!(
            Config::new("d")
                .apply_args(args("--rendezvous ftp://x"))
                .is_err()
        );
    }

    #[test]
    fn the_project_rendezvous_is_a_valid_rendezvous() {
        crate::rendezvous::validate(crate::PROJECT_RENDEZVOUS).unwrap();
        assert!(crate::PROJECT_RENDEZVOUS.starts_with("https://"));
        assert!(!crate::PROJECT_RENDEZVOUS.ends_with('/'));
        assert_eq!(
            crate::DEFAULT_RENDEZVOUS.is_some(),
            cfg!(feature = "project-rendezvous")
        );
    }
}
