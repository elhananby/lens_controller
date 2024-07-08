use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};
use reqwest::blocking::Client;
use serde_json::Value;
use url::Url;
use log::{info, warn, error, debug};
use env_logger::Env;

use lens_driver::LensDriver;

#[derive(Debug, Clone)]
pub struct KalmanEstimatesRow {
    pub obj_id: u32,
    pub frame: u32,
    pub timestamp: f32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub xvel: f64,
    pub yvel: f64,
    pub zvel: f64,
}

#[derive(Debug)]
enum BraidEvent {
    Birth(KalmanEstimatesRow),
    Update(KalmanEstimatesRow),
    Death { obj_id: u32 },
}

struct LensController {
    braid_url: Url,
    client: Client,
    lens_driver: LensDriver,
    tracking_zone: TrackingZone,
    currently_tracked_obj: Option<u32>,
    last_update_time: Instant,
    update_interval: Duration,
}

#[derive(Debug, Clone)]
struct TrackingZone {
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    z_min: f64,
    z_max: f64,
}

impl LensController {
    fn new(braid_url: &str, lens_driver_port: &str, tracking_zone: TrackingZone, update_interval_ms: u64) -> Result<Self, Box<dyn Error>> {
        let url = Url::parse(braid_url)?;
        let client = Client::new();
        let lens_driver = LensDriver::new(Some(lens_driver_port.to_string()));

        // Verify connection
        client.get(url.clone()).send()?;
        
        Ok(Self {
            braid_url: url,
            client,
            lens_driver,
            tracking_zone,
            currently_tracked_obj: None,
            last_update_time: Instant::now(),
            update_interval: Duration::from_millis(update_interval_ms),
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

    fn is_in_zone(&self, row: &KalmanEstimatesRow) -> bool {
        row.x >= self.tracking_zone.x_min && row.x <= self.tracking_zone.x_max &&
        row.y >= self.tracking_zone.y_min && row.y <= self.tracking_zone.y_max &&
        row.z >= self.tracking_zone.z_min && row.z <= self.tracking_zone.z_max
    }

    fn handle_event(&mut self, event: BraidEvent) {
        match event {
            BraidEvent::Birth(row) | BraidEvent::Update(row) => {
                if self.is_in_zone(&row) {
                    if self.currently_tracked_obj.is_none() || self.currently_tracked_obj == Some(row.obj_id) {
                        self.currently_tracked_obj = Some(row.obj_id);
                        self.update_lens_position(row.z);
                        info!("Tracking object {}: z = {}", row.obj_id, row.z);
                    }
                } else if self.currently_tracked_obj == Some(row.obj_id) {
                    info!("Object {} left the tracking zone", row.obj_id);
                    self.currently_tracked_obj = None;
                }
            },
            BraidEvent::Death { obj_id } => {
                if self.currently_tracked_obj == Some(obj_id) {
                    info!("Tracked object {} died", obj_id);
                    self.currently_tracked_obj = None;
                }
            }
        }
    }

    fn update_lens_position(&mut self, z: f64) {
        let now = Instant::now();
        if now.duration_since(self.last_update_time) >= self.update_interval {
            debug!("Updating lens position for z = {}", z);
            // self.lens_driver.update_position(z);
            self.last_update_time = now;
        } else {
            debug!("Skipping lens update due to time constraint");
        }
    }

    fn run(&mut self) -> Result<(), Box<dyn Error>> {
        let events_url = self.braid_url.join("events")?;
        let response = self.client
            .get(events_url)
            .header("Accept", "text/event-stream")
            .send()?;

        let mut reader = BufReader::new(response);
        let mut buffer = String::new();

        loop {
            buffer.clear();
            match reader.read_line(&mut buffer) {
                Ok(0) => break,  // End of stream
                Ok(_) => {
                    if let Some(stripped) = buffer.strip_prefix("data: "){

                        let data: Value = serde_json::from_str(stripped)?;
                        
                        if let Some(msg) = data["msg"].as_object() {
                            if let Some((event_type, event_data)) = msg.iter().next() {
                                let event = match event_type.as_str() {
                                    "Birth" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Birth),
                                    "Update" => Self::parse_kalman_estimates(event_data).map(BraidEvent::Update),
                                    "Death" => event_data["obj_id"].as_u64().map(|obj_id| BraidEvent::Death { obj_id: obj_id as u32 }),
                                    _ => None,
                                };

                                if let Some(event) = event {
                                    self.handle_event(event);
                                } else {
                                    warn!("Received unknown event type: {}", event_type);
                                }
                            }
                        }
                    }
                },
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let args: Vec<String> = env::args().collect();
    let braid_url = args.get(1).map(String::as_str).unwrap_or("http://10.40.80.6:8397/");
    let lens_driver_port = args.get(2).map(String::as_str).unwrap_or("/dev/optotune_ld");
    let update_interval_ms = args.get(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20); // Default to 100ms if not provided
    
    let tracking_zone = TrackingZone {
        x_min: -10.0, x_max: 10.0,
        y_min: -10.0, y_max: 10.0,
        z_min: 0.0, z_max: 100.0,
    };

    info!("Initializing LensController with URL: {}, port: {}, update interval: {}ms", 
          braid_url, lens_driver_port, update_interval_ms);
    let mut lens_controller = LensController::new(braid_url, lens_driver_port, tracking_zone, update_interval_ms)?;
    
    info!("Starting LensController");
    if let Err(e) = lens_controller.run() {
        error!("LensController encountered an error: {}", e);
    }

    Ok(())
}