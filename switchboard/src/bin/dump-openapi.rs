use treadmill_switchboard::routes::openapi_spec;

fn main() {
    let api = openapi_spec();

    // YAML: matches the committed snapshot format (api-spec/openapi.yaml).
    print!("{}", serde_norway::to_string(&api).unwrap());
}
