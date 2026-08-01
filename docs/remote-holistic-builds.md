# Remote Holistic Builds

VW builds require at least two different types of machines today

1. A Linux machine that has vivado installed for building FPGA images
2. A Helios machine for building kernel modules.

The Oxide Cloud Computer is the perfect substrate to execute these
multi-instance builds. But that means we need to create a vw service
that can manage VW build environments composed of multiple underlying
instances.

The concept for this looks like the following

```
                       ┌───────────────────────────────────────────────┐
                       │             Oxide Cloud Computer              │
                       │                                               │
                       │              ┌───────────────────────────┐    │
                       │              │        environment        ├┐   │
               src,    │              │ ┌──────────┐ ┌──────────┐ │├┐  │
┌────────┐   commands  │  ┌────────┐  │ │  vivado  │ │  helios  │ │││  │
│        │─────────────┼─▶│        │  │ │ instance │ │ instance │ │││  │
│ vw-cli │             │  │ vw-svc │  │ └──────────┘ └──────────┘ │││  │
│        │◀────────────┼─ │        │  │       ┌──────────┐        │││  │
└────────┘   results   │  └────────┘  │       │ artifact │        │││  │
                       │              │       │ instance │        │││  │
                       │              │       └──────────┘        │││  │
                       │              └┬──────────────────────────┘││  │
                       │               └┬──────────────────────────┘│  │
                       │                └───────────────────────────┘  │
                       │                                               │
                       └───────────────────────────────────────────────┘
  
```

We add  new `vw-svc` crate. It's a binary create that produces a daemon
that exposes an API server for

1. Managing build environments.
2. Carrying out tasks within those build environments.

In the diagram above, each build environment contains a vivado instance,
a helios instance, and an artifact instance. The artifact instance is
an S3 server that provides a place for build outputs to go, and also
a place for intermediate artifacts the be shared between instances.

An example workflow could look something like this.

1. Set up a remote environment with `vw cloud init <name>`.
2. Now just use `vw` like it's used today, `vw run` executes `design.htcl`
   `vw repl` takes the user into the repl. But when a cloud environment
  has been initialized, all of this takes place remotely on the vivado instance.
  Same thing for `vw bench`, it will feel the same as it does today, but it will
  execute on the vivado instance in the cloud.

This obviously means that we'll need a background daemon that synchronizes sources
from the local machine running the `vw` client to the `vw-svc` daemon which will
then distribute the sources to the backend instances they need to go to. The `vw`
cli should manage that daemon directly, the user should not need to muck with it.

We'll also need to build an artifact daemon for the vivado and helios instances
that watch `target` directories and ship artifacts to the artifact instance S3
server.

Something that's important about this system is that it be interactive. A call
to `vw run` cannot be no output for an hour+ while a synth/place/route run is
going. We need to stream data back in real time to the client and deliver the
same experience as the local tool has today.
