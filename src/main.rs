use std::fmt::Debug;
use std::net::ToSocketAddrs as _;

use goobus::GooBus;
use gooey::value::{Dynamic, Switchable, Validations};
use gooey::widget::{Children, MakeWidget};
use gooey::widgets::input::InputValue;
use gooey::widgets::progress::Progressable;
use gooey::widgets::slider::Slidable;
use gooey::Run;

#[derive(Debug, PartialEq, Eq)]
struct Config {
    name: String,
    listen_on: String,
}

fn main() -> gooey::Result {
    let config = Dynamic::new(None::<Config>);

    config
        .switcher(|config, dynamic| {
            if let Some(config) = config {
                let bus = GooBus::new(&config.name, Some(&config.listen_on));

                bus_ui(bus).make_widget()
            } else {
                startup_ui(dynamic).make_widget()
            }
        })
        .centered()
        .run()
}

fn startup_ui(config: &Dynamic<Option<Config>>) -> impl MakeWidget {
    let name = Dynamic::<String>::default();
    let listen_on = Dynamic::from("::1:0");
    let validations = Validations::default();

    "Node Name:"
        .and(name.clone().into_input())
        .and("Listen On:")
        .and(
            listen_on
                .clone()
                .into_input()
                .validation(validations.validate(&listen_on, |listen_on| {
                    listen_on
                        .to_socket_addrs()
                        .map(|_| ())
                        .map_err(|err| err.to_string())
                })),
        )
        .and(
            "Start"
                .into_button()
                .on_click(validations.when_valid({
                    let config = config.clone();
                    let name = name.clone();
                    let listen_on = listen_on.clone();
                    move |()| {
                        config.set(Some(Config {
                            name: name.get(),
                            listen_on: listen_on.get(),
                        }));
                    }
                }))
                .into_default(),
        )
        .into_rows()
}

fn bus_ui(bus: GooBus) -> impl MakeWidget {
    let my_value = Dynamic::new(0_u8);
    bus.register("value", &my_value);

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

            if max_index + 1 < connected_peers.len() {
                connected_peers.truncate(max_index + 1);
            }
        }
    })
    .persist();

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

    let connect_to = Dynamic::<String>::default();

    format!("{} listening on {}", bus.name(), bus.listening_on())
        .and(
            connect_to
                .clone()
                .into_input()
                .placeholder("Connect To")
                .and("+".into_button().on_click(move |()| {
                    // TODO validation
                    if let Ok(addr) = connect_to.take().parse() {
                        bus.connect_to(addr);
                    }
                }))
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
