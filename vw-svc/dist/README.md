# Running vw-svc

Deployment files for vw-svc on a systemd host.

| File | Installed to |
| --- | --- |
| [`vw-svc.service`](vw-svc.service) | `/etc/systemd/system/vw-svc.service` |
| [`vw-svc.env.example`](vw-svc.env.example) | `/etc/vw-svc/vw-svc.env`, mode 0600, once |
| [`install.sh`](install.sh) | — |

## Install

```sh
cargo build --release -p vw-svc
sudo ./vw-svc/dist/install.sh
```

Or from a build CI already did, which is the same binary the images get their
agent from:

```sh
sudo ./vw-svc/dist/install.sh --commit <sha>
```

The first install leaves the service enabled and stopped, because the
configuration it just wrote is the example. Edit `/etc/vw-svc/vw-svc.env`, then
`systemctl start vw-svc`.

Re-running is safe. The binary and the unit are replaced; the configuration
file never is. A running service is **not** restarted unless you pass
`--restart` — vw-svc relays the connections builds run over, so a restart ends
whatever synthesis, REPL session or artifact download is in flight, and when to
do that is your call.

## Configuration

Everything site-specific is in `/etc/vw-svc/vw-svc.env`: which certificate to
serve, which rack to provision on, who administers the service. The unit reads
it and nothing else, so reinstalling never disturbs how a machine is set up.

Each `VW_SVC_*` variable is split on whitespace into arguments, so values may
not contain spaces or quoting. `OXIDE_TOKEN` is the exception — vw-svc reads
that one from the environment by name.

## Certificates

vw-svc serves TLS from a certificate on disk and watches it. Get one from
Let's Encrypt:

```sh
certbot certonly --standalone -d vw.example.com
```

Then point `VW_SVC_TLS` at the `live/` symlinks, not at the files under
`archive/`. Renewals are certbot's own systemd timer (`certbot.timer`, twice
daily, a no-op until a certificate is within 30 days of expiry) — there is no
deploy hook to configure and nothing to restart. vw-svc notices the replaced
certificate within a minute and serves it from the next handshake on;
connections already established are untouched.

The service runs as root for this reason: certbot keeps `/etc/letsencrypt/live`
and `archive` at `0700 root` and re-creates them on each renewal, so any
group-readable arrangement made once does not survive. The unit is sandboxed
accordingly — read-only filesystem, no home, restricted syscalls.

### Testing renewal before it happens for real

Renewal will not fire for about two months, and `certbot renew --dry-run`
writes to a temporary directory, so it never touches the files vw-svc watches.
To exercise the whole path now:

```sh
certbot renew --force-renewal
journalctl -u vw-svc | grep -i certificate
```

Expect `certificate replaced` followed by `now serving the replaced
certificate` from both `user_api` and `admin_api`, within a minute, with the
PID unchanged. Once is enough: `--force-renewal` counts against Let's Encrypt's
limit of five duplicate certificates per week.
