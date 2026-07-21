# HoloTCL

## Background

TCL, the Tool Command Language is heavily used to drive Electronic Design
Automation (EDA) software. The typical TCL interface to an EDA software
suite includes not just commands to automate EDA workflows, but also commands
to configure complex intellectual property (IP) packages, inspect designs,
analyze the outputs of the EDA processes that carry a design from source to
implementation such as synthesized netlists, and provide critical parameters and
constraints at multiple stages of the automation process that collectively make
complex electronic designs realizable.

For designs built on EDA platforms that have a TCL interface, TCL code is a
major part of the engineering process and the overall codebase that needs to
be maintained. The TCL interface that engineers have to work against is quite
large. At the time of writing, Vivado 2025.1, the EDA suite for AMD FPGAs has
around 900 TCL procedures for automation. While certainly non-trivial, the
automation procedures pale in comparison to IP configuration interfaces. A
complex IP can have thousands of interrelated configuration parameters.

The engineering process for both automation, IP configuration and design
parameterization revolves around a few core questions.

1. Discovery: what interfaces are available?
2. Semantics: how are those interfaces intended to be used?
3. Structure: what is the shape of those interfaces?
4. Correctness: have I used those interfaces correctly?

For automation functions, most EDA suites' TCL will give you something
for discovery, semantics and structure. There is a TCL shell that provides
rudimentary command completion and command help menus that informally describe
what the arguments for each command are and give you a vague idea of what
parameters might be acceptable, maybe even with some examples. But because of
TCL's "everything is a string" view of the world, there are fundamental limits
on the amount of structure that can be communicated through a TCL interface
description.

While it's possible to get by with TCL for EDA process automation. Where the
wheels really fall off is IP configuration. Configuring via TCL is done through
dictionaries where the keys are strings and values are basically anything:
strings, lists of strings, nested dictionaries, lists of dictionaries, lists of
lists etc. These dictionary configuration interfaces are undocumented. While
almost all IP comes with PDF-based documentation, and some come with sections
on the IP parameterization. Experience has shown that these documents are
nowhere near complete, are often just straight up wrong, and provide no
real basis for discovery, semantics, structure or anything close to correctness
validation.

The EDA disposition is to provide GUIs for IP configuration that emit TCL behind
the scenes. Alas, even these GUIs suffer from the same issues as the TCL itself.
There is no comprehensive documentation for their configuration, and using them
for IP configuration devolves to a guessing game rather than deliberate
engineering. Compounding these issues is that GUI-based configuration that
generates TCL is a lossy channel. An engineer cannot be expected to make a
decision in a GUI, reverse engineer how that decision manifests in generated TCL
that uses a completely undocumented interface and then annotate the generated
TCL, the only engineering artifact they can even put in source control, with the
rationale for their configuration decision, or the way in which they decided to
integrate the IP in to a broader design. Not to mention that the next time they
make changes in the GUI and emit TCL, _new_ TCL will get emitted and the engineer
must merge this with their existing corpus of annotated TCL. Put differently
the GUI to TCL channel is an intrinsically lossy one. A lossy channel that emits
code using an undiscoverable, undocumented and unstructured interface.

## Rationale

HoloTCL is an evolution of TCL that aims to make EDA interfaces discoverable,
not just for the simple notion of discovering what procedures and parameters are
available, but discovery of interfaces with semantics and structural definition
build into the interface definitions themselves. Providing a foundation for
tools that can ensure structural and many forms of static correctness by
analyzing an HTCL codebase rather than having to execute an EDA process only to
find simple syntactic, structural or semantic errors hours into a design run.

This is accomplished with the following foundations.

- The IP configuration interface moves from unstructured dictionaries to
  configuration procedures.

- HTCL introduces documentation comments and requires them for all procedures
  and procedure arguments.

- All procedure arguments are named keyword arguments in HTCL.

- HTCL introduces a basic type system, with standard primitive types,
  enumerations and type parameterized lists and dictionaries.

- Procedure arguments are typed, and have return types.

The idea behind all of this is that the language requires by construction
that the EDA interfaces engineers use are documented and well structured by
construction.
