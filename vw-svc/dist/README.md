# Running vw-svc

Deployment files for vw-svc on a systemd host.

| File | Installed to |
| --- | --- |
| [`vw-svc.service`](vw-svc.service) | `/etc/systemd/system/vw-svc.service` |
| [`vw-svc.env.example`](vw-svc.env.example) | `/etc/vw-svc/vw-svc.env`, mode 0600, once |
| [`vw-svc-beta.service`](vw-svc-beta.service) | `/etc/systemd/system/vw-svc-beta.service` |
| [`vw-svc-beta.env.example`](vw-svc-beta.env.example) | `/etc/vw-svc-beta/vw-svc-beta.env`, mode 0600, once |
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

## The beta service

`--beta` installs a second, complete vw-svc beside the production one on the
same host, so a build of the service can be exercised against real work
without production being the thing that is being tested.

```sh
sudo ./vw-svc/dist/install.sh --beta --commit <sha>
```

Nothing is shared. The beta gets its own binary, unit, configuration file,
state directory and ports:

| | production | beta |
| --- | --- | --- |
| binary | `/usr/local/bin/vw-svc` | `/usr/local/bin/vw-svc-beta` |
| unit | `vw-svc.service` | `vw-svc-beta.service` |
| configuration | `/etc/vw-svc/vw-svc.env` | `/etc/vw-svc-beta/vw-svc-beta.env` |
| database | `/var/lib/vw-svc/` | `/var/lib/vw-svc-beta/` |
| user API | 2727 | 2828 |
| admin API | 2728 | 2829 |
| objects on the rack | `vwsvc-*` | `vwsvcbeta-*` |
| images | its project and the silo | its project only |

Because the names differ all the way down, `install.sh --beta` and
`install.sh` cannot reach each other's files: upgrading or restarting the beta
leaves production running the binary it was running, and the reverse.

The beta's ports are fixed in `vw-svc-beta.service` rather than left to its
environment file. Which port a deployment answers on is what makes it the beta
rather than something a site chooses, so it sits in the unit next to
`--db-path`. The practical consequence is that `VW_SVC_EXTRA` in
`/etc/vw-svc-beta/vw-svc-beta.env` must not name `--user-api-port`,
`--admin-api-port` or `--db-path` — clap refuses a second occurrence, so the
service stops at startup naming the flag. Copying production's `VW_SVC_EXTRA`
across verbatim is the way to trip this.

Point a client at the beta with the same knobs that select any other service:

```sh
export VW_SVC_URL=https://vw.example.com:2828
export VW_SVC_ADMIN_URL=https://vw.example.com:2829
```

or `vw cloud --url https://vw.example.com:2828` for a single command.

### How the two stay apart on the rack

The reconciler decides what to keep by diffing the rack against its own
database and reclaiming every object of its own that is left over. The beta's
database does not contain production's environments, so without something
separating them, production's instances, disks and keys are all orphans to the
beta — and the beta's are orphans to production.

What separates them is the names. `vw-svc-beta.service` passes `--beta`, and
the service then creates and recognizes `vwsvcbeta-*` instead of `vwsvc-*`:

| | production | beta |
| --- | --- | --- |
| instance | `vwsvc-ferris-alpha-vivado` | `vwsvcbeta-ferris-alpha-vivado` |
| boot disk | `vwsvc-ferris-alpha-vivado` | `vwsvcbeta-ferris-alpha-vivado` |
| silo ssh key | `vwsvc-ferris-alpha` | `vwsvcbeta-ferris-alpha` |

Ownership everywhere is "the name starts with `{prefix}-`", so the two sets are
disjoint in both directions and neither reaper can see the other's objects.
That is why the prefix is `vwsvcbeta` and not the more readable `vwsvc-beta`,
which *would* start with `vwsvc-` and so belong to production; there is a test
in `oxide.rs` whose job is to stop anyone changing it back.

**The beta may therefore share production's endpoint and `OXIDE_TOKEN`.**
Sharing the token is in fact the case the naming exists for: the silo ssh key
list belongs to the token's user and is not scoped by project at all, so no
arrangement of projects would have kept two deployments off each other's keys.

### The beta needs its own project, for the images

Images are the one thing the naming does not separate, and the one thing that
must be separated anyway.

An image is named for the kind it boots — `vw-vivado-*`, `vw-helios-*`,
`vw-artifact-*` — by whoever built it. Nothing in the name says which
deployment it belongs to, so **the project is the only thing that does**. And
the images are not interchangeable: each carries the vw-agent that vw-svc talks
to, and a beta service exists precisely so that side of the API can differ from
production's. Compatibility across the two is not guaranteed and is not meant
to be.

The failure is quiet. A kind left unnamed resolves to the *newest* image
matching its prefix, so an environment that can see the other deployment's
images will simply boot one. Nothing fails at create time — the instance comes
up, and the agent inside it answers a protocol the service on the other end
does not speak.

So `--oxide-project` must name a project holding the beta's images, with all
three kinds published into it. Two things then hold:

- **The beta ignores silo images.** A silo image is visible from every project,
  which from the beta's side makes it by definition somebody else's. Its
  project is the complete and only account of what it can boot; an image
  visible in `oxide image list` but not in that project will not be picked, and
  naming it explicitly is refused. Production is unchanged and still sees both.
- **Image recycling stays scoped.** It deletes only project images, so each
  deployment recycles its own.

The beta says which project it boots from on every start:

```text
the beta boots only images in its own project    project=redhawk-beta
```

which is what to check first if environment creation is failing with `no image
matching 'vw-vivado-*' is visible to this service`.

Until the rack settings are uncommented the beta records environments and
provisions nothing, which is what an unconfigured install should do and what it
says in the log. That is a fine state to leave it in while it has nothing to
do.

### Removing the beta

```sh
sudo systemctl disable --now vw-svc-beta
```

Delete every environment the beta owns first, through its admin API — stopping
the service stops the reconciler, and the instances and disks it created
outlive it. They are the `vwsvcbeta-*` ones, so what to clear up by hand is
unambiguous. Once the rack is clear,
`/usr/local/bin/vw-svc-beta`, `/etc/systemd/system/vw-svc-beta.service`,
`/etc/vw-svc-beta` and `/var/lib/vw-svc-beta` are all there is to remove, and
none of them is production's.

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

The beta serves the same certificate: it is the same host under the same name,
and only the port differs. Nothing about reading a certificate is exclusive —
both services follow the live symlinks and both pick up a renewal on their own,
neither needing a restart for one.

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
