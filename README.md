[![codecov](https://codecov.io/gh/dragazo/netsblox-vm/graph/badge.svg?token=W60PECO0UQ)](https://codecov.io/gh/dragazo/netsblox-vm)

[NetsBlox](https://netsblox.org/) is a browser-based visual programming environment (based on [Snap!](https://snap.berkeley.edu/)) that extends the typical block-based programming features with internet and distributed computing tools such as RPCs (remote procedure calls) that access network resources (e.g., databases or web APIs), and message passing between projects over the internet.

`netsblox-vm` is a native Rust implementation of the NetsBlox code execution engine and runtime. This makes it possible to execute NetsBlox program code at blistering speeds on any device targeted by the Rust compiler. But more importantly, `netsblox-vm` has several extension APIs which make it possible to extend its features with new hardware peripheral access capabilities (e.g., controlling sensors and actuators through GPIO), all with safe, native Rust.

## Features

`netsblox-vm` contains the following feature flags.

| name | default | description |
| ---- | ------- | ----------- |
| `std`  | on | Enables the `std` crate dependency and access to the default [`StdSystem`](crate::std_system::StdSystem) implementation of [`System`](crate::runtime::System) |
| `cli` | on | Enables the `std` feature flag and additionally gives access to the [`cli`](crate::cli) submodule, which gives API access to the standard CLI (needed for syscall extensions) rather than having to write a CLI from scratch |
| `serde` | on | Enables serialization of some types |
| `native-tls` | on | Enables the `native-tls` feature for TLS-capable dependencies (only used if `std` is also enabled) |
| `native-tls-vendored` | off | Enables the `native-tls-vendored` feature for TLS-capable dependencies (only used if `std` is also enabled) |
| `rustls-tls-native-roots` | off | Enables the `rustls-tls-native-roots` feature for TLS-capable dependencies (only used if `std` is also enabled) |
| `rustls-tls-webpki-roots` | off | Enables the `rustls-tls-webpki-roots` feature for TLS-capable dependencies (only used if `std` is also enabled) |

Note that if `std` is enabled, one of the TLS feature flags must also be enabled in order to connect to the NetsBlox server with [`StdSystem`](crate::std_system::StdSystem).
The `native-tls` feature is enabled by default to support this on common desktop and server environments;
however you may need to disable default features and explicitly opt into a different TLS option for other targets (e.g., Android or iOS).

## `no-std`

`netsblox-vm` supports building in `no-std` environments by disabling the default `std` feature flag.
However, the `alloc` crate is still required in this case.

```toml
[dependencies]
netsblox_vm = { version = "...", default-features = false }
```

Note that this precludes access to [`StdSystem`](crate::std_system::StdSystem), meaning a new implementation of [`System`](crate::runtime::System) would be required for your target platform.

## CLI Installation

This crate includes a binary called `nb` which serves as a shallow wrapper for the [`cli`](crate::cli) API with a default suite of syscall extensions.
_Note: if you need to create your own syscall extensions, you must do so in a separate binary using this crate as a dependency._

```bash
cargo install netsblox-vm
```
