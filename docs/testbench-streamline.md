# Streamlining Testbenches

Currently running a testbench requires the following procedure.

```
mkdir -p target/anodizer/build
mkdir -p target/anodizer/generated
anodizer gen_structs --build-dir target/anodizer/build --output target/anodizer/build
````
