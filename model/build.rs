fn main() {
    prost_build::compile_protos(
        &[
            "proto/company.proto",
            "proto/market.proto",
            "proto/earnings.proto",
            "proto/ticker.proto",
        ],
        &["proto/"],
    )
    .expect("failed to compile proto files");
}
