This is ratatui-image 4.2.0, vendored from crates.io under its MIT license (see LICENSE).
LazyDon's sole source change removes the automatic `tmux set -p allow-passthrough on`
command in `src/picker.rs`. Existing tmux settings are left to the user; image
protocol passthrough still requires an appropriate tmux configuration.
