


// protobuf stuff

fn main() {
    prost_build::compile_protos(
        &["proto/message.proto", "proto/challenge.proto"],
        &["proto/"]
    ).unwrap();

}