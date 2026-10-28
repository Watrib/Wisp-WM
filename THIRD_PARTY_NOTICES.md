# Third-party notices

The Wayland protocol and input-grab plumbing in `src/compositor/` is adapted
from Smithay 0.7.0's Smallvil example:
<https://github.com/Smithay/smithay/tree/v0.7.0/smallvil>.
Additional protocol integration follows Smithay's Anvil example:
<https://github.com/Smithay/smithay/tree/v0.7.0/anvil>.
The direct backend's session/input wiring, GBM/EGL setup, VT shortcut handling,
and Xcursor loading/rendering follow Anvil's `udev.rs`, `input_handler.rs`,
`cursor.rs`, and `drawing.rs` at the same tag:
<https://github.com/Smithay/smithay/tree/v0.7.0/anvil/src>.
The embedded fallback cursor mask in Wisp is original project code.
The local icon compatibility implementation is adapted from Smithay's
`src/wayland/xdg_toplevel_icon.rs`:
<https://github.com/Smithay/smithay/blob/v0.7.0/src/wayland/xdg_toplevel_icon.rs>.

Smithay is distributed under the following license. Other dependencies retain
their respective licenses; see `Cargo.lock` and the crates' metadata.

```text
MIT License

Copyright (c) 2017 Victor Berger and Victoria Brekenfeld

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
