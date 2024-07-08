use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader};
use reqwest::blocking::Client;
use serde_json::Value;
use url::Url;

mod types;
use types::KalmanEstimatesRow;

use lens_driver::LensDriver;

#[derive(Debug)]
enum BraidEvent {
    Birth (KalmanEstimatesRow),
    Update (KalmanEstimatesRow),
    Death { obj_id: u32 },
}

struct LensController {
    braid_url: Url,
    client: Client,
    lens_driver: LensDriver,
}

impl LensController {
    fn new(braid_url: &str, lens_driver_port: &str) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;
        let client = Client::new();
        let lens_driver = LensDriver::new(Some(lens_driver_port.to_string()));

        // Verify connection
        client.get(url.clone()).send()?;
        
        Ok(Self {
            braid_url: url,
            client,
            lens_driver,
        })
    }

    fn parse_kalman_estimates(data: &Value) -> Option<KalmanEstimatesRow> {
        Some(KalmanEstimatesRow {
            obj_id: data["obj_id"].as_u64()? as u32,
            frame: data["frame"].as_u64()? as u32,
            timestamp: data["timestamp"].as_f64()? as f32,
            x: data["x"].as_f64()?,
            y: data["y"].as_f64()?,
            z: data["z"].as_f64()?,
            xvel: data["xvel"].as_f64()?,
            yvel: data["yvel"].as_f64()?,
            zvel: data["zvel"].as_f64()?,
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
                
                // if let Some(version) = data["v"].as_u64() {
                //     assert_eq!(version, 3, "Unsupported data version");
                // }

                if let Some(msg) = data["msg"].as_object() {
                    if let Some((event_type, event_data)) = msg.iter().next() {
                        let event = match event_type.as_str() {
                            "Birth" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Birth),
                            "Update" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Update),
                            "Death" => event_data["obj_id"].as_u64().map(|obj_id| BraidEvent::Death { obj_id: obj_id as u32 }),
                            _ => None,
                        };
                        println!("{:?}", event_type);
                        println!("{:?}", event_data);
                    }

                }
            }

        }

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = env::args().collect();
    let braid_url = args.get(1).map(String::as_str).unwrap_or("http://10.40.80.6:8397/");
    let lens_driver_port = args.get(2).map(String::as_str).unwrap_or("/dev/optotune_ld");
    let lens_controller = LensController::new(braid_url, lens_driver_port)?;
    lens_controller.run()?;

    Ok(())
}