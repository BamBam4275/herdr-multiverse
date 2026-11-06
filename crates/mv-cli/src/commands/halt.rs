//! `multiverse halt on|off|status`: the fleet-wide halt switch.
//!
//! The on-disk format and read/write/clear logic live in
//! [`mv_state::halt`] — this module is only the clap wiring: resolving the
//! config and state dir, parsing the subcommand, and printing the result.

use clap::ArgMatches;
use mv_state::halt::{
    clear_halt_state, format_halt_line, read_halt_state, write_halt_state, HaltState,
};

use super::doctor::{resolve_config_path, state_dir_from_config, utc_timestamp};
use crate::exit::Failure;

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let state_dir = state_dir_from_config(&config_path);
    let (sub_name, sub_args) = args.subcommand().expect("clap requires a halt subcommand");

    match sub_name {
        "on" => {
            let reason = sub_args.get_one::<String>("reason").cloned();
            let state = HaltState {
                halted: true,
                reason,
                at: Some(utc_timestamp()),
            };
            write_halt_state(&state_dir, &state).map_err(|error| {
                Failure::Operation(format!("could not write halt state: {error}"))
            })?;
            println!("{}", format_halt_line(&state));
            Ok(())
        }
        "off" => {
            let cleared = clear_halt_state(&state_dir).map_err(|error| {
                Failure::Operation(format!("could not clear halt state: {error}"))
            })?;
            println!(
                "{}",
                if cleared {
                    "fleet halt cleared."
                } else {
                    "fleet was not halted."
                }
            );
            Ok(())
        }
        "status" => {
            println!("{}", format_halt_line(&read_halt_state(&state_dir)));
            Ok(())
        }
        other => Err(Failure::Operation(format!(
            "halt: unknown subcommand {other}"
        ))),
    }
}
