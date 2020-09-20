<h1 align="center">async-metronome</h1>
<div align="center">
 <strong>
   Unit testing framework for async Rust
 </strong>
</div>

<br />

<div align="center">
  <!-- Crates version -->
  <a href="https://crates.io/crates/async-metronome">
    <img src="https://img.shields.io/crates/v/async-metronome.svg?style=flat-square"
    alt="Crates.io version" />
  </a>
  <!-- Downloads -->
  <a href="https://crates.io/crates/async-metronome">
    <img src="https://img.shields.io/crates/d/async-metronome.svg?style=flat-square"
      alt="Download" />
  </a>
  <!-- docs.rs docs -->
  <a href="https://docs.rs/async-metronome">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="docs.rs docs" />
  </a>
</div>

<br/>

This crate implements a async unit testing framework, which is based on the  **MutithreadedTC** developed by William Pugh and Nathaniel Ayewah at the University of Maryland.

> *"MultithreadedTC is a framework for testing concurrent applications. It features a 
> metronome that is used to provide fine control over the sequence of activities in
> multiple threads." [...](https://www.cs.umd.edu/projects/PL/multithreadedtc/overview.html)*

Concurrent applications are often not deterministic, and so are failures. Depending on the interleavings of threads, they may appear and disappear.

With **async-metronome**, it is possible to define test cases that demonstrate a fixed interleaving of threads. If concurrent abstractions under test are reasonably small, it should be feasible to create multiple test cases that will deterministically cover all relevant interleavings.

The current implementation is based on [async-std](https://crates.io/crates/async-std), but in the future, it should be possible to support [tokio](https://tokio.rs) as well.

To avoid copying original documentation here, please check [MultithreadedTC](https://www.cs.umd.edu/projects/PL/multithreadedtc/) web site, and the corresponding [paper](https://www.cs.umd.edu/projects/PL/multithreadedtc/pubs/ASE07-pugh.pdf).

## Example

```rust
use async_metronome::{assert_tick, await_tick};

async fn test_send_receive() {
    let test = async {
        let (mut sender, mut receiver) = mpsc::channel::<usize>(1);

        let sender = async move {
            assert_tick!(0);
            sender.send(42).await.unwrap();
            sender.send(17).await.unwrap();
            assert_tick!(1);
        };
        let receiver = async move {
            assert_tick!(0);
            await_tick!(1);
            receiver.next().await;
            receiver.next().await;
        };
        
        let sender = async_metronome::spawn(sender);
        let receiver = async_metronome::spawn(receiver);

        sender.await;
        receiver.await;
    };

    async_metronome::run(test).await;
}

```
## Explanation (adapted from the MutithreadedTC)

async-metronome has an internal clock. The clock only advances to the next tick when all tasks are in a pending state.

The clock starts at `tick 0`. In this example, the macro `await_tick!(1)` makes the receiver block until the clock reaches `tick 1` before resuming. Thread 1 is allowed to run freely in `tick 0`, until it blocks on the call to `sender.send(17)`. At this point, all threads are blocked, and the clock can advance to the next tick.

In `tick 1`, the statement `receiver.next(42)` in the receiver is executed, and this frees up sender. The final statement in sender asserts that the clock is in `tick 1`, in effect asserting that the task blocked on the call to `sender.send(17)`.

Complete example, can be found in the [tests/send_receive.rs](tests/send_receive.rs).

## Installation
```sh
$ cargo add async-metronome
```

<div align="center">
    <strong style="color: #a00">
    This is the initial pre-alpha release. Bugs in testing tools are especially dangerous - use with care.
    </strong>
</div>

## License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br/>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
 


