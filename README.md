# Vigil

A screen-awake applet for the COSMIC™ desktop. Sits in the panel and prevents your screen from sleeping — with preset durations or indefinite mode.

> This project is created and maintained by Ben Gubler ([@bgub](https://github.com/bgub)). It is not affiliated with or endorsed by System76.

![Screenshot](screenshot.png)

## Installation

### Build from source

Requires a [Rust toolchain][rustup], [just][just], and the standard COSMIC™ build dependencies (`pkg-config`, plus dev libraries for dbus, wayland, xkbcommon, etc. — the same packages needed to build any COSMIC™ applet).

```sh
just install        # system-wide (/usr), needs sudo
just install-user   # user-local (~/.local), no root needed
```

Then add the applet to a panel in **Settings > Desktop > Panel**.

### NixOS

NixOS users should use `just install-user` to install to `~/.local`.

## Allow screen to sleep

With this on, Vigil keeps the system from suspending but lets the screen turn
off on its usual timeout, for leaving a machine reachable over SSH or remote
desktop. It holds a logind `sleep` block instead of a screensaver inhibit.

On systemd older than 257 (Pop!_OS 24.04 included), logind ignores a block
held by the same user who asks to suspend, and COSMIC's idle daemon suspends
as you with a plain `systemctl suspend`. So the first time you turn the
setting on, Vigil adds one entry to your shortcuts config,
`~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/system_actions`:

```ron
Suspend: "systemctl suspend --check-inhibitors=yes",
```

That makes the idle suspend check every block, including your own. It takes
effect at your next login; until then the popup says so. If you already have
your own `Suspend` command, Vigil leaves it alone and warns you instead. On
systemd 257 and later nothing is added, because logind refuses by itself.

The effect lasts only as long as Vigil holds the block, which logind releases
when the process exits for any reason, so the machine goes back to suspending
on its normal schedule once Vigil is off or gone. The idle daemon does not
retry a suspend it was refused, so when Vigil lets go after holding the block
for at least the suspend timeout, it restarts the daemon's countdown and the
machine suspends that long afterwards. After a shorter session no attempt was
refused, and the countdown already running is left alone.

The entry is harmless when no block is held. To take it out, turn the setting
off and press **Remove Vigil's Suspend command** in the popup;
`just uninstall-user` and `just uninstall` do it for you. A Flatpak uninstall
cannot run anything, so there, remove the `Suspend:` line from that file by
hand, or run `cosmic-ext-applet-vigil --remove-suspend-override` before
uninstalling.

## Translators

[Fluent][fluent] is used for localization of the software. Fluent's translation files are found in the [i18n directory](./i18n). New translations may copy the [English (en) localization](./i18n/en) of the project, rename `en` to the desired [ISO 639-1 language code][iso-codes], and then translations can be provided for each [message identifier][fluent-guide]. If no translation is necessary, the message may be omitted.

## Packaging

If packaging for a Linux distribution, vendor dependencies locally with the `vendor` rule, and build with the vendored sources using the `build-vendored` rule. When installing files, use the `rootdir` and `prefix` variables to change installation paths.

```sh
just vendor
just build-vendored
just rootdir=debian/cosmic-ext-applet-vigil prefix=/usr install
```

It is recommended to build a source tarball with the vendored dependencies, which can typically be done by running `just vendor` on the host system before it enters the build environment.

## Developers

Developers should install [rustup][rustup] and configure their editor to use [rust-analyzer][rust-analyzer].

[fluent]: https://projectfluent.org/
[fluent-guide]: https://projectfluent.org/fluent/guide/hello.html
[iso-codes]: https://en.wikipedia.org/wiki/List_of_ISO_639-1_codes
[just]: https://github.com/casey/just
[rustup]: https://rustup.rs/
[rust-analyzer]: https://rust-analyzer.github.io/
