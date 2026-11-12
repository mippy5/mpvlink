# Deploying the signaling server

Everyone in a room has to reach the same signaling server. For use on one
machine or one home network, `make signal` is enough. For friends over the
internet, run it on a server with a public address, behind HTTPS.

The server only introduces peers: it relays a few KB of connection details per
join (codecs, network addresses, encryption fingerprints). Video goes directly
between peers and never touches it, so the smallest VPS is plenty.

```
 mpv (viewer) ──wss──┐                         ┌── Caddy :443 (TLS) ──► mpvlink-signal 127.0.0.1:8787
 mpvlink-send ──wss──┴──► signal.example.com ──┘
        └────────────── video: peer to peer (WebRTC) ──────────────┘
```

## What you need

- A Linux server with a public IP. These steps assume Ubuntu 22.04+/Debian 12+
  on x86_64. (For ARM servers, you'd need an aarch64 build; `make linux` only
  has an x86_64 toolchain set up so far.)
- A domain name, with a DNS `A` record (and `AAAA` if the server has IPv6)
  pointing a hostname at the server, e.g. `signal.example.com`. Caddy needs this
  to get a certificate. (No domain? See
  [Using an IP address instead of a domain](#using-an-ip-address-instead-of-a-domain).)
- SSH access with sudo.

Below, replace `signal.example.com` with your hostname and `you@server` with
your SSH login.

## 1. Build (on your Mac)

```sh
make linux PKG=mpvlink-signal
```

This produces `dist/linux-x86_64/mpvlink-signal`. (See the Makefile for the
one-time cross-compiler setup if it isn't installed.)

## 2. Install the server as a service

Copy the binary and the systemd unit over:

```sh
scp dist/linux-x86_64/mpvlink-signal deploy/mpvlink-signal.service you@server:/tmp/
```

On the server:

```sh
sudo install -m 755 /tmp/mpvlink-signal /usr/local/bin/mpvlink-signal
sudo install -m 644 /tmp/mpvlink-signal.service /etc/systemd/system/mpvlink-signal.service
sudo systemctl daemon-reload
sudo systemctl enable --now mpvlink-signal
systemctl status mpvlink-signal     # should say "active (running)"
```

The unit runs it as a throwaway user (`DynamicUser`), listening on
`127.0.0.1:8787` only, with most system access locked down.

## 3. Put Caddy in front for HTTPS

Install Caddy from its official repository (distro packages can be too old for
the config below, which needs Caddy 2.7+). Follow
<https://caddyserver.com/docs/install#debian-ubuntu-raspbian>, then:

```sh
scp deploy/Caddyfile you@server:/tmp/Caddyfile        # from your Mac
```

On the server, edit the hostname in `/tmp/Caddyfile`, then:

```sh
sudo install -m 644 /tmp/Caddyfile /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Caddy fetches a certificate on first use and renews it automatically.

## 4. Firewall

Allow SSH, HTTP (for certificate issuance) and HTTPS. Don't open port 8787:
only Caddy should talk to the signaling server.

```sh
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw enable
```

If your provider also has a cloud firewall (security groups etc.), open 80 and
443 there too.

## 5. Check it works

From your Mac, a WebSocket handshake should get `101 Switching Protocols`:

```sh
curl -si --http1.1 --max-time 3 \
  -H "Connection: Upgrade" -H "Upgrade: websocket" \
  -H "Sec-WebSocket-Version: 13" -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
  https://signal.example.com/ | head -1
```

Then a real end-to-end test: stream the test pattern into a room...

```sh
cargo run --release -p mpvlink-send -- --server wss://signal.example.com --room TEST42
```

...and watch it in mpv (ideally from another network, e.g. a phone hotspot,
to also test NAT traversal):

```sh
mpv --idle=yes --script=target/release/mpvlink.so \
    --script-opts=mpvlink-server=wss://signal.example.com
```

## 6. Point mpv at it permanently

On each person's machine, open the mpvlink menu (`Ctrl+j`), click
**Settings** (or press `Ctrl+,`), enter `wss://signal.example.com` and save.
It's remembered across launches. (Windows users: see [windows.md](windows.md)
for installing the plugin.)

Alternatively, put it in `~/.config/mpv/mpv.conf`. This overrides the saved
setting (the Settings page will say so):

```
script-opts-append=mpvlink-server=wss://signal.example.com
```

## Day-to-day

**Logs** (joins, leaves, errors):

```sh
journalctl -u mpvlink-signal -f
```

These include client IP addresses, display names and room codes. journald
keeps them according to its retention settings (`/etc/systemd/journald.conf`).

**Updating**: rebuild with `make linux PKG=mpvlink-signal`, copy the binary
over as in step 2, then:

```sh
sudo install -m 755 /tmp/mpvlink-signal /usr/local/bin/mpvlink-signal
sudo systemctl restart mpvlink-signal
```

A restart briefly disconnects everyone from the server. Clients reconnect on
their own (retrying after 0.5, 1, 2, 4 and 8 seconds, then every 15 seconds),
and streams in progress keep playing meanwhile, since video is peer to peer.
Because a restarted server has forgotten who was where, publishers then set
up their connections to viewers again, which shows up as a hiccup of a second
or two in the video.

Shorter interruptions (a Caddy reload, a network blip on either side) are
invisible: the server holds each client's place for 30 seconds, and a client
that reconnects within that time picks up exactly where it was.

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| `curl` gets `502 Bad Gateway` | The service isn't running: `systemctl status mpvlink-signal`. |
| `curl`/client TLS errors right after setup | Certificate not issued yet: DNS doesn't point at the server, or port 80/443 is blocked. Check `journalctl -u caddy`. |
| mpv keeps saying "Lost connection to the server. Reconnecting…" | The server is down or unreachable (`systemctl status mpvlink-signal`, `journalctl -u caddy`). The current stream keeps playing; clients reconnect on their own once it's back. |
| Signaling works (you see "X started streaming. Connecting…") but no video | The peers can't reach each other directly, typically a strict NAT/firewall. This needs a TURN relay, which mpvlink doesn't support yet. |
| `This room already has someone streaming.` | Rooms allow one publisher. If the previous one crashed, the server holds its place for 30 s in case it reconnects. If it vanished without closing its connection (power or network loss), the server first takes up to 90 s to notice. Try again after that. |

## Using an IP address instead of a domain

Since January 2026, Let's Encrypt issues certificates for IP addresses
([announcement](https://letsencrypt.org/2026/01/15/6day-and-ip-general-availability/)),
so clients can connect to `wss://203.0.113.5` with no domain name. Before
choosing this, know the trade-offs:

- **They last about 6 days** (Let's Encrypt's `shortlived` profile is mandatory
  for IP certificates), so renewal has to be automatic. Each renewal reloads
  Caddy; mpvlink clients reconnect and resume on their own, so nobody notices.
- **They're tied to the IP.** If your server's address changes, the
  certificate stops matching until a new one is issued, and everyone has to
  update their Settings. A hostname avoids both problems; even a free
  dynamic-DNS name works with the main setup above.

Caddy can in principle get IP certificates itself, but there are reports of it
failing ([caddy#7399](https://github.com/caddyserver/caddy/issues/7399)), so this
setup has [Certbot](https://certbot.eff.org/) get the certificate and Caddy
just use it. Certbot needs version **5.4 or newer** for IP certificates with
the webroot method
([Let's Encrypt's Certbot post](https://letsencrypt.org/2026/03/11/shorter-certs-certbot/)),
which is newer than most distro packages.

Below, replace `203.0.113.5` with your server's public IP. (For IPv6, write it
in brackets in URLs and the Caddyfile, e.g. `wss://[2001:db8::1]`.)

**1. Do steps 1, 2 and 4 above** (build, install the service, firewall). Port
80 must be open: Let's Encrypt checks it to confirm you control the IP.

**2. Install Certbot** from the snap, which tracks the latest release:

```sh
sudo snap install --classic certbot
sudo ln -sf /snap/bin/certbot /usr/bin/certbot
certbot --version                   # needs 5.4 or newer
```

**3. Install Caddy** (as in step 3) and start with a config that only serves
Certbot's challenge files on port 80. (The full config can't load yet,
because the certificate it points to doesn't exist.)

```sh
sudo mkdir -p /var/www/acme
printf 'http://203.0.113.5 {\n\troot * /var/www/acme\n\tfile_server\n}\n' | sudo tee /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

**4. Install the deploy hook**, which copies each new certificate to where
Caddy can read it and reloads Caddy. From your Mac:

```sh
scp deploy/certbot-deploy-hook.sh deploy/Caddyfile.ip you@server:/tmp/
```

On the server:

```sh
sudo install -m 755 /tmp/certbot-deploy-hook.sh /usr/local/bin/mpvlink-caddy-cert-hook
```

**5. Get the certificate.** First a dry run against Let's Encrypt's staging
environment (nothing is saved), then the real thing:

```sh
sudo certbot certonly --dry-run --preferred-profile shortlived \
  --webroot --webroot-path /var/www/acme --ip-address 203.0.113.5

sudo certbot certonly --preferred-profile shortlived \
  --webroot --webroot-path /var/www/acme --ip-address 203.0.113.5 \
  --deploy-hook /usr/local/bin/mpvlink-caddy-cert-hook
```

Certbot remembers these settings, including the hook, for renewals. The hook
runs right away too, so `/etc/caddy/certs/` should now hold the certificate.

**6. Switch Caddy to the full config**, which also serves the signaling server
on port 443 with that certificate:

```sh
sed 's/203\.0\.113\.5/YOUR.IP.HERE/g' /tmp/Caddyfile.ip | sudo tee /etc/caddy/Caddyfile
caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

**7. Check renewal is automatic.** The snap sets up a timer that runs twice a
day. A dry run checks the renewal itself works (it skips the deploy hook):

```sh
systemctl list-timers | grep certbot
sudo certbot renew --dry-run
```

**8. Test from your Mac** with mpvlink's own client. `curl` is a weaker test,
since it uses your Mac's certificate store, while mpvlink has its own built in:

```sh
cargo run --release -p mpvlink-send -- --server wss://203.0.113.5 --room TEST42
```

"Streaming to room TEST42…" means the certificate is accepted. Then set
`wss://203.0.113.5` in mpv's Settings (step 6 of the main setup).

## Security notes

- Anyone who knows the server's address can create and join rooms. The room
  code is the only access control: generated codes are 6 characters from a
  31-character alphabet (~887 million combinations), but codes people type in
  themselves may be easier to guess.
- There's no rate limiting yet. Messages are capped at 64 KB.
- The server sees IP addresses, display names and room codes, never video.
