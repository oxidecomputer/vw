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

On Oxide, the whole host is usually built from
[`vw-cloud-images/deploy/vw-svc`](https://github.com/oxidecomputer/vw-cloud-images)
rather than by hand, and that Terraform calls this script from cloud-init. What
is here is what a machine ends up with either way.

## Configuration

Everything site-specific is in `/etc/vw-svc/vw-svc.env`: which deployment this
is, which certificate to serve, which project to provision in, who administers
the service. The unit reads it and nothing else, so reinstalling never disturbs
how a machine is set up.

Each `VW_SVC_*` variable is split on whitespace into arguments, so values may
not contain spaces or quoting. `OXIDE_TOKEN` is the exception — vw-svc reads
that one from the environment by name.

## A deployment is a project

One vw deployment — `prod`, `beta`, or somebody else's — is one Oxide project,
and everything belonging to it is inside: this service's own instance, the
images environments boot, and the environment instances themselves.

That is not a convention, it is a constraint. vw-svc reaches agents on their
**private VPC addresses**, and a VPC does not span projects. A service outside
the project it provisions into cannot talk to anything it creates.

So there is one vw-svc per host and one per project, and `--oxide-project` in
`/etc/vw-svc/vw-svc.env` is what says which deployment this is more than
anything else does.

### The deployment name

`--deployment` is the one setting with no default, and the service refuses to
start without it. Everything this service creates is named
`vwsvc-{deployment}-…`, and only those names are recognized as its own:

| | `prod` | `beta` |
| --- | --- | --- |
| instance | `vwsvc-prod-ferris-alpha-vivado` | `vwsvc-beta-ferris-alpha-vivado` |
| boot disk | `vwsvc-prod-ferris-alpha-vivado` | `vwsvc-beta-ferris-alpha-vivado` |
| silo ssh key | `vwsvc-prod-ferris-alpha` | `vwsvc-beta-ferris-alpha` |

If the project already separates instances and disks, why does the name matter?
Because of the third row. **The silo ssh key list belongs to the token's user
and is not scoped by project at all**, so two deployments on one `OXIDE_TOKEN`
walk the same list. The reconciler reclaims every key of its own that no
environment wants, and without the name in it, every one of the other
deployment's keys looks exactly like that.

Two rules follow, and both are enforced at startup:

- **The name must be unique across the silo**, not merely across a project. Two
  deployments called `prod` in different projects will reap each other's keys.
- **The name may not contain a hyphen.** Ownership is "the name starts with
  `vwsvc-{deployment}-`", so a deployment called `prod-west` would have every
  one of its objects claimed — and then deleted — by one called `prod`. There
  is a test in `oxide.rs` that exists to keep this true.

By convention the name matches the project, which is already silo-unique:
project `vw-prod` runs deployment `prod`.

### Images come from the project, never the silo

An image is named for the kind it boots — `vw-vivado-*`, `vw-helios-*`,
`vw-artifact-*` — by whoever built it. Nothing in the name says which
deployment it belongs to, so **the project is the only thing that does**. And
they are not interchangeable: each carries the vw-agent this service talks to,
and a second deployment exists precisely so that side of the API can differ.

The failure is quiet. A kind left unnamed resolves to the *newest* image
matching its prefix, so a deployment that could see another's images would
simply boot one. Nothing fails at create time — the instance comes up, and the
agent inside answers a protocol the service on the other end does not speak.

So a silo image is by definition somebody else's, and is skipped. The project
is the complete and only account of what a deployment can boot: an image
visible in `oxide image list` but not in the project will not be picked, and
naming it explicitly is refused. Image recycling is scoped the same way, so
each deployment recycles only its own.

Every start says which project that is:

```text
booting only images in this deployment's project    project=vw-prod
```

which is the first thing to check if environment creation fails with `no image
matching 'vw-vivado-*' is visible to this service`.

Until the rack settings are uncommented, the service records environments and
provisions nothing — which is what an unconfigured install should do, and what
it says in the log.

## Ports

| | port | |
| --- | --- | --- |
| user API | 443 | so the service is reached as `https://{host}`, with no port to quote |
| admin API | 2053 | a separate listener, so who may reach it is decided separately |

Both are defaults in the binary rather than settings in the unit. Binding 443
needs privilege the service already has for the certificate.

Clients pick a deployment by URL and nothing else:

```sh
vw cloud list                                   # https://vw-cloud.dev
export VW_SVC_URL=https://beta.vw-cloud.dev     # or another deployment
vw cloud --url https://vw.example.com list      # for one command
```

The admin API does not follow `--url`, since it is a separate listener on a
separate port: `--admin-url` or `VW_SVC_ADMIN_URL` names it.

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

Port 80 has to be reachable for `--standalone` to answer the challenge, which
on Oxide means a firewall rule for it in the deployment's VPC. vw-svc itself
does not use 80.

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
