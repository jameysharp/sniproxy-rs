# sniproxy

This is a reverse proxy for running multiple TLS services on the same IP
address and port. It does not decrypt the application data, but just
looks at the [Server Name Indication (SNI)][SNI] extension of the
unencrypted TLS `ClientHello` message to determine which backend to
forward the connection to.

[SNI]: https://en.wikipedia.org/wiki/Server_Name_Indication

Only Unix-like systems, with support for Unix domain sockets and passing
sockets as file descriptors, are supported. A Windows-friendly version
probably wouldn't be too hard, but I can't test it.

# Invocation

This program doesn't take any command line options, and doesn't have a
configuration file in a traditional sense.

First off, you have to pass a listening TCP socket as its stdin. There
are many ways to do that; the configuration examples below may help as a
starting point. If you need to listen on multiple sockets, run a
separate copy of sniproxy for each socket.

Second, create a directory to hold your backend configuration,
containing a subdirectory for each hostname you want to serve. The
working directory for sniproxy must be this top-level configuration
directory. Hostnames must be represented without a trailing dot (`.`)
and in lowercase ASCII. (So for international domain names, use the
"[A-label][]" form.)

[A-label]: https://tools.ietf.org/html/rfc5890#section-2.3.2.1

Each hostname subdirectory can have these files:

- `tls-socket` (required): a Unix domain socket that your backend
  application is listening for connections on.

- `send-proxy-v1`: if present (even empty), every connection forwarded
  for this host will be prefixed by a [PROXY protocol][] v1 header.

[PROXY protocol]: https://www.haproxy.org/download/2.1/doc/proxy-protocol.txt

For example, if your configuration directory is called `hosts/`, and
you're hosting a web site at "[🕸💍.ws][webring]", then you'd put the
backend socket for that site in `hosts/xn--sr8hvo.ws/tls-socket`, and
run sniproxy with the `hosts` directory as its current working
directory.

[webring]: https://🕸💍.ws

That's it! You have a reverse proxy now.

## Tips and tricks

If you want, you can allow users on your server to configure new
hostnames without any administrator intervention. Set the configuration
directory either to mode 1777 (like `/tmp`) or to mode 1775, to
authorize everyone or just people in a specific group. By setting the
"sticky bit", only the person who created a hostname can delete it.
And because sniproxy only looks for a canonical version of the hostname
(no trailing dot, all lowercase, ASCII compatible encoding), there's no
way that two people can register the same hostname; the filesystem
enforces uniqueness.

If instead you want every new hostname to get reviewed by an
administrator first, then you can (and probably should) still delegate
configuration of each hostname to different user accounts, just using
standard Unix ownership and permissions on the per-hostname directories.
Then the operating system ensures that people can't reconfigure
hostnames they aren't authorized to manage.

You may want to have a look at `acl(5)` for how to give the user which
sniproxy runs under access to everything in your configuration
directory, even for those directories which are not owned by that
sniproxy user or group.

You can change the configuration without restarting sniproxy: it looks
up the target socket each time a connection comes in, and doesn't need
to know which hostnames to serve in advance. This is especially
important if untrusted users will be managing any hostname
configurations because allowing them to restart or reload the reverse
proxy is risky.

The configuration files or directories can also be symlinks, which
allows a few tricks.

- If multiple hostnames should be served by the same backend, they can
  all be symlinked to one directory, rather than configuring the backend
  to listen on many sockets.

- If `tls-socket` is a symlink to the actual socket, then you can
  atomically replace the symlink to change which backend process serves
  that hostname, without downtime.

This program is designed to be able to run in a minimal chroot
environment, but if you use symlinks in your configuration make sure
that they are relative links which stay inside the chroot.

Because the client's raw TLS protocol stream is forwarded to the backend
unmodified, your backend can negotiate any TLS options you want it to,
including:

- application protocol negotiation via [ALPN][], such as for HTTP/2
- the ACME (Let's Encrypt) [tls-alpn-01][] verification method
- optional or required client certificates
- pre-shared keys
- TLS versions or extensions which haven't been invented yet

[ALPN]: https://en.wikipedia.org/wiki/Application-Layer_Protocol_Negotiation
[tls-alpn-01]: https://tools.ietf.org/html/rfc8737

To stop the proxy gracefully, send it a `SIGHUP` signal. It will stop
listening for new connections, so you can start a new copy immediately,
but it will wait up to ten seconds for existing connections to close
before exiting.

# Building

This project is implemented in [Rust][] so you need [Cargo][] installed.
The conventional `cargo build` or [`cargo install`][install] commands
work, but you might want some additional options.

[Rust]: https://www.rust-lang.org/
[Cargo]: https://doc.rust-lang.org/cargo/getting-started/installation.html
[install]: https://doc.rust-lang.org/book/ch14-04-installing-binaries.html

If you want to statically link your build of sniproxy and you're on
Linux, you can build with musl libc. This can make deployment easier
since the resulting program is a single file with no external
dependencies. If you installed Rust using `rustup`, you should be able
to run commands something like this to get a static binary:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --target=x86_64-unknown-linux-musl --release
```

Replace `x86_64` with the output of `uname -m` from whichever computer you
want to run this program on. It doesn't even have to be the same as the
architecture where you have Rust installed: see `rustc --print
target-list` for the list of targets you can compile for.

# Configuration examples

## systemd

If you use systemd, you can configure a pair of units to run sniproxy.

A socket unit might look like this, in `sniproxy.socket`:

```ini
[Socket]
ListenStream=443
```

If you've statically linked sniproxy, then it should work even under
fairly strict isolation, as in this example `sniproxy.service` unit:

```ini
[Unit]
Requires=sniproxy.socket

[Service]
Type=exec
StandardInput=socket
KillSignal=SIGHUP
TimeoutStopSec=10s

RootDirectoryStartOnly=yes
RootDirectory=/srv/sniproxy
ExecStartPre=+cp /usr/bin/sniproxy /srv/sniproxy
ExecStart=./sniproxy
ExecStartPost=-+rm /srv/sniproxy/sniproxy

User=sniproxy
ProtectSystem=yes
MountAPIVFS=no
PrivateNetwork=yes
RestrictAddressFamilies=AF_UNIX
MemoryDenyWriteExecute=yes
SystemCallFilter=@system-service
SystemCallArchitectures=native
```

## inetd

The classic "internet super-server" can run sniproxy when the first
connection comes in, with a line in `/etc/inetd.conf` like this:

```
https stream tcp wait sniproxy /usr/bin/sniproxy sniproxy
```

## s6

The process supervision suite [s6][] provides tools for setting up a
process like this, in conjunction with [s6-networking][]. Here's a
sample script written using the [execline][] language for listening on
IPv6; a similar script covers IPv4 as well.

[s6]: http://skarnet.org/software/s6/
[s6-networking]: http://skarnet.org/software/s6-networking/
[execline]: http://skarnet.org/software/execline/

```sh
#!/usr/bin/env -S execlineb -P
s6-tcpserver6-socketbinder :: 443
s6-setuidgid sniproxy
cd /srv/sniproxy
/usr/bin/sniproxy
```
