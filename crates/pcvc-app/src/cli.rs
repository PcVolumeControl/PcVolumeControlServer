//! Command-line flags.

pub const USAGE: &str = "\
usage: pcvolumecontrol [--show] [--mock-audio]

  --show         open the window on launch (a second launch always shows
                 the running instance's window and exits)
  --mock-audio   use the scripted mock audio backend instead of the system one
  --version      print the version and exit
  --help         print this help and exit

Logging follows RUST_LOG (default: info).";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Args {
    pub show: bool,
    pub mock_audio: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
    Version,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut parsed = Args::default();
    for arg in args {
        match arg.as_str() {
            "--show" => parsed.show = true,
            "--mock-audio" => parsed.mock_audio = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Parsed::Run(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Parsed, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn flags() {
        assert_eq!(parse_strs(&[]), Ok(Parsed::Run(Args::default())));
        assert_eq!(
            parse_strs(&["--mock-audio", "--show"]),
            Ok(Parsed::Run(Args {
                show: true,
                mock_audio: true
            }))
        );
        assert_eq!(parse_strs(&["--show", "--help"]), Ok(Parsed::Help));
        assert_eq!(parse_strs(&["-V"]), Ok(Parsed::Version));
        assert!(parse_strs(&["--port"]).is_err());
    }
}
