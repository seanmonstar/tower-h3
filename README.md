# tower-h3

This is a WIP Rust library for providing HTTP/3 through tower's `async fn(Req) -> Res` model.

It's essentially [h3](https://github.com/hyperium/h3) + [tower](https://github.com/tower-rs/tower).

## Purpose

To experiment before `h3` is available in [hyper](https://hyper.rs). Similar to what we did with [tower-h2](https://github.com/tower-rs/tower-h2).
