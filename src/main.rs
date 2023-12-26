use std::io;
use std::net::SocketAddr;
use std::str::FromStr;

use goobus::GooBus;
use gooey::styles::components::{ErrorColor, TextColor};
use gooey::value::{Dynamic, Switchable, Validations};
use gooey::widget::{Children, MakeWidget};
use gooey::widgets::input::InputValue;
use gooey::widgets::progress::Progressable;
use gooey::widgets::slider::Slidable;
use gooey::Run;
use intentional::Assert;

fn main() -> gooey::Result {
    let name = Dynamic::<String>::default();
    let listen_on = Dynamic::from("[::1]:0");
    let run = Dynamic::new(false);

    run.switcher(move |run, run_dynamic| {
        if *run {
            match GooBus::new(name.get(), Some(listen_on.get())) {
                Ok(bus) => bus_ui(bus).make_widget(),
                Err(err) => error_launching(&err, run_dynamic).make_widget(),
            }
        } else {
            startup_ui(&name, &listen_on, run_dynamic).make_widget()
        }
    })
    .centered()
    .run()
}

fn startup_ui(
    name: &Dynamic<String>,
    listen_on: &Dynamic<String>,
    run: &Dynamic<bool>,
) -> impl MakeWidget {
    let validations = Validations::default();
    let run = run.clone();

    "Node Name:"
        .and(name.clone().into_input())
        .and("Listen On:")
        .and(
            listen_on
                .clone()
                .into_input()
                .validation(validations.validate(listen_on, |listen_on| {
                    listen_on
                        .parse::<SocketAddr>()
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                })),
        )
        .and(
            "Start"
                .into_button()
                .on_click(validations.when_valid({
                    move |()| {
                        run.set(true);
                    }
                }))
                .into_default(),
        )
        .into_rows()
}

fn bus_ui(bus: GooBus) -> impl MakeWidget {
    let my_value = Dynamic::new(0_u8);
    bus.publish("value", &my_value);

    // Gather the list of peer names. This callback happens while the bus state
    // is locked, preventing us from adding subscriptions.
    let connected_peers = Dynamic::<Vec<String>>::default();
    bus.on_state_change({
        let connected_peers = connected_peers.clone();
        move |state| {
            let mut connected_peers = connected_peers.lock();
            let mut max_index = 0;
            for (index, peer) in state.peers().enumerate() {
                max_index = index;
                if Some(&peer.name) != connected_peers.get(index) {
                    // New peer at this location
                    connected_peers.insert(index, peer.name.clone());
                }
            }

            // If we have any peers left, they've been disconnected.
            if max_index + 1 < connected_peers.len() {
                connected_peers.truncate(max_index + 1);
            }
        }
    })
    .persist();

    // Create a slider for each connected peer, subscribing to their value on
    // the bus.
    let peers = connected_peers.map_each({
        let bus = bus.clone();
        move |peer_names| {
            peer_names
                .iter()
                .map(|peer| {
                    peer.as_str()
                        .and(bus.subscribe_to::<u8>(peer, "value").progress_bar())
                        .into_rows()
                        .make_widget()
                })
                .collect::<Children>()
        }
    });

    let validations = Validations::default();
    let connect_to = Dynamic::<String>::default();
    let connect_to_address = connect_to.map_each(|connect_to| {
        let trimmed = connect_to.trim();
        if trimmed.is_empty() {
            Err(String::from("Connection address required"))
        } else {
            SocketAddr::from_str(trimmed).map_err(|err| err.to_string())
        }
    });

    format!("{} listening on {}", bus.name(), bus.listening_on())
        .and(
            connect_to
                .clone()
                .into_input()
                .placeholder("Connect To")
                .validation(validations.validate_result(connect_to_address.clone()))
                .and(
                    "+".into_button()
                        .on_click(validations.when_valid(move |()| {
                            let addr = connect_to_address.get().assert("validations ran");
                            bus.connect_to(addr);
                        })),
                )
                .into_rows(),
        )
        .into_rows()
        .and(
            "Local Value:"
                .and(my_value.slider())
                .and(peers.into_rows())
                .into_rows()
                .expand(),
        )
        .into_columns()
}

fn error_launching(error: &io::Error, run: &Dynamic<bool>) -> impl MakeWidget {
    let run = run.clone();
    "Error starting the network bus:"
        .and(error.to_string().with_dynamic(&TextColor, ErrorColor))
        .and("Ok".into_button().on_click(move |()| run.set(false)))
        .into_rows()
}
