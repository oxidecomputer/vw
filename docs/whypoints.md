# Why

Why HTCL

1. Structured, documented discoverable interface.
2. Catch common programming errors at analysis time.

The H stands for holomorphic, meaning this type of TCL captures the whole
shape of a tool's interface.

## Notes

1. Documentation versus engineering reality
  - There is always a detla between Xilinix product guides and the configuation
    surface reality.
  - DCMAC `tx_data_out_*` is an example of this. It's not mentioned anywhere in
    PG 369. Other examples are abundant.
  - Tightening up PDF documentation is not the answer. Decoupled PDF as a means
    of documenting an engineering interface is a failure by design.
  - The interface that engineers actually use must itself be documented, it's
    only way to do this.

2. Structured interfaces
  - Configuring IP is complex, both in the number of parameters available and
    how those paramters are structured as an overall configuration.
  - Structured interfaces make it clear what configuration opions there are
    and how they can be composed.
  - Structured interfaces enforce structurally correct configurations by
    construction, e.g. they make it impossible to compose structurually invalid
    configurations.
  - Building structure into interfaces empowers analyzers to catch many clases
    of bugs.
    - Incorrect assignment of values can be caught by the type system through
      enums and composite types.
    - Subtle issues like multiple assignment of the same paramter can be caught
      at compile/analysis time rather than runtime (actually *running* an IP
      configuration and synthesis script can take hours)
    - Configuration key typos manifest as compile/analysis time errors.
  - Structured interfaces enable discoverability, through tools like LSPs as
    well as documentation generators. When configuration takes place through
    functions with well documented arguments, the design surface of interest
    is readily discoverable by the engineer. And critically, there are no more
    guessing games on what the right paramter actually is.
  - The analyzer can catch and warn about unused variables. Forgetting to
    actually use a variable can lead to subtle bugs that can take hours if not
    days to catch for complex designs.
  - Configuration options as typed enumerations is extremely powerful
    - If a config option just takes a string, it's
      1. Not at all clear what the valid options are
      2. Even if you think you know the valid options, it's easy to be wrong or
         make a type
    - Typed enumerations take away both problems entirely.
      - They structually define what values are valid, making them *discoverable*
      - This means analyzers can catch invalid issues, not the runtime after an
        hour of running.
      - It provides a natural surface for documenting the input alternatives.

3. Catch common programming errors at analysis time
  - Design build scripts can take an enormous amount of time to run.
  - There are many things that can be caught by basic analysis and not having
    to run them at all.
    - Functions that declare a return type but:
      - Return nothing
      - Return the wrong type
      - Don't _always_ return the right type
    - Passing incorrect arguments types to procs
    - Passing the wrong arguments to procs
    - Calling undefined procs
    - Refrencing undefined variables
    - Sourcing dependencies that don't exist
    - Supplying enumeration values as strings with typos
  - Catching these hours into a built is a punch in the face that is completely
    unnecessary and avoidable.
  - HTCL catches all of these at analysis time in seconds.
