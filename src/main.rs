use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader};
use reqwest::blocking::Client;
use serde_json::Value;
use url::Url;

#[derive(Debug)]
enum BraidEvent {
    Birth { obj_id: String, x: f64, y: f64, z: f64 },
    Update { obj_id: String, x: f64, y: f64, z: f64 },
    Death { obj_id: String },
}

struct LensController {
    braid_url: Url,
    client: Client,
}

impl LensController {
    fn new(braid_url: &str) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;
        let client = Client::new();
        
        // Verify connection
        client.get(url.clone()).send()?;
        
        Ok(Self {
            braid_url: url,
            client,
        })
    }

    fn run(&self) -> Result<(), Box<dyn Error>> {
        let events_url = self.braid_url.join("events")?;
        let response = self.client
            .get(events_url)
            .header("Accept", "text/event-stream")
            .send()?;

        let reader = BufReader::new(response);
        let mut buffer = String::new();

        for line in reader.lines() {
            buffer = line?;
            if buffer.starts_with("data: ") {
                let data: Value = serde_json::from_str(&buffer["data: ".len()..])?;
                
                if let Some(version) = data["v"].as_u64() {
                    assert_eq!(version, 2, "Unsupported data version");
                }

                if let Some(msg) = data["msg"].as_object() {
                    let event = msg.iter().next().and_then(|(event_type, data)| {
                        match event_type.as_str() {
                            "Birth" => Some(BraidEvent::Birth {
                                obj_id: data["obj_id"].as_str().unwrap_or("").to_string(),
                                x: data["x"].as_f64().unwrap_or(0.0),
                                y: data["y"].as_f64().unwrap_or(0.0),
                                z: data["z"].as_f64().unwrap_or(0.0),
                            }),
                            "Update" => Some(BraidEvent::Update {
                                obj_id: data["obj_id"].as_str().unwrap_or("").to_string(),
                                x: data["x"].as_f64().unwrap_or(0.0),
                                y: data["y"].as_f64().unwrap_or(0.0),
                                z: data["z"].as_f64().unwrap_or(0.0),
                            }),
                            "Death" => Some(BraidEvent::Death {
                                obj_id: data["obj_id"].as_str().unwrap_or("").to_string(),
                            }),
                            _ => None,
                        }
                    });

                }
            }
        }

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let braid_url = args.get(1).map(String::as_str).unwrap_or("http://127.0.0.1:8397/");

    let lens_controller = LensController::new(braid_url)?;
    lens_controller.run()?;

    Ok(())
}