# GooBus

**This is just a demo/example. It is not meant to be used in production code.**

This repository contains a standalone application powered by [Gooey][gooey]'s
reactive system that enables a peer-to-peer network of value publishing and
subscribing, loosely inspired by the Ivy software bus.

## GooBus Library

The API surface looks like this:

- `GooBus::new(name, bind) -> GooBus`: Initializes a new bus agent with the given name and
  bind address.
- `GooBus.connect_to(address)`: Attempts to connect to an address. Upon
  successful connection and handshake, both peers notify all their current peers
  of the new peer.
- `GooBus.register(name, &dynamic)`: Registers a dynamic value with the bus
  using the name. Peers will be able to subscribe to updates of this value using
  the given name.
- `GooBus.subscribe_to<T>(peer, name) -> Dynamic<T>`: Subscribes to a value from
  a given peer. This dynamic will be initialized with `T::default()`, and
  whenever the peer is connected to the bus and values are received, the dynamic
  will be updated.

## GooBus Executable

Upon launching the app, it asks for a name and a bind address. Once launched,
the left hand side allows connecting to the address of another client.

[gooey]: https://github.com/khonsulabs/gooey
