use aide::openapi::{Info, OpenApi};
use treadmill_switchboard::routes::api_router;

fn main() {
    let mut api = OpenApi {
        info: Info {
            title: "Treadmill Switchboard API".to_string(),
            version: "0.1.0".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    let _ = api_router().finish_api(&mut api);

    println!("{}", serde_json::to_string_pretty(&api).unwrap());
}
