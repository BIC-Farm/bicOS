// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use ii_logging::macros::*;

use bosminer_erupter::config;

use bosminer_config::clap;
use bosminer_config::{ClientDescriptor, ClientUserInfo};

#[tokio::main]
async fn main() {
    let app = clap::App::new(bosminer::SIGNATURE)
        .version(bosminer::version::STRING.as_str())
        .arg(
            clap::Arg::with_name("pool")
                .short("p")
                .long("pool")
                .value_name("HOSTNAME:PORT")
                .help("Address the stratum V2 server")
                .required(true)
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("user")
                .short("u")
                .long("user")
                .value_name("USERNAME.WORKERNAME[:PASSWORD]")
                .help("Specify user and worker name")
                .required(true)
                .takes_value(true),
        );

    let matches = app.get_matches();
    let _log_guard =
        ii_logging::setup_for_app(bosminer_erupter::config::ASYNC_LOGGER_DRAIN_CHANNEL_SIZE);

    let url = matches
        .value_of("pool")
        .expect("BUG: missing 'pool' attribute");
    let user_info = matches
        .value_of("user")
        .expect("BUG: missing 'user' attribute");
    let user_info = ClientUserInfo::parse(user_info);

    let backend_config =
        config::Backend::new(match ClientDescriptor::create(url, &user_info, true) {
            Err(e) => {
                error!("Cannot set pool from command line: {}", e.to_string());
                return;
            }
            Ok(v) => v,
        });

    ii_async_utils::setup_panic_handling();
    bosminer::main::<bosminer_erupter::Backend>(backend_config, bosminer::SIGNATURE.to_string())
        .await;
}
