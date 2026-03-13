use serde_json::Value;
use serde::{Serialize, Deserialize};
use anyhow::Result;
use reqwest::{header::{AUTHORIZATION, HeaderValue, HeaderMap}};

#[derive(Clone, Serialize, Deserialize)]
pub struct OilDisplay {
    pub price: f64,
    pub change_ammount: f64,
    pub change_percent: f64,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct OilPrice {
    api: String,
    headers: HeaderMap,
    pub price: f64,
    pub change_ammount: f64,
    pub change_percent: f64,
    pub updated_at: String,

}
impl OilPrice {
    pub fn new() -> Self {
        let api = "https://api.oilpriceapi.com/v1/prices/latest".to_string();
        let token = "d5d5e437119e12f25633c0802e931db9a12c1087a14ef151f0848e40bc0806b4".to_string();
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&format!("Token {}", token)).unwrap());
        Self {
            api,
            headers,
            price: 0.0,
            change_ammount: 0.0,
            change_percent: 0.0,
            updated_at: String::new(),
        }
    }
    pub fn display(&self) -> OilDisplay{
        OilDisplay { price: self.price, change_ammount: self.change_ammount,
             change_percent: self.change_percent, updated_at: self.updated_at.clone() }
    }
    pub async fn update(&mut self) -> Result<()> {
        let client = reqwest::Client::new();
        let raw_data = match client.get(&self.api)
        .headers(self.headers.clone())
        .send().await {
            Ok(n) => match n.text().await {
                Ok(n) => n,
                Err(e) => {
                    println!("Error converting data to text: {}",e);
                    "ERROR CONVERING DATA".to_string()
                }
            },
            Err(e) => {
                println!("Error while getting API data: {}", e);
                "INVALID DATA".to_string()
            },
        };
        let data: Value = serde_json::from_str(&raw_data).unwrap();
        if let Some(data) = data.get("data") {
            if let Some(time_stamp) = data.get("updated_at").and_then(Value::as_str) {
                println!("UPDATED AT: {}", time_stamp);
                self.updated_at = time_stamp.to_string();
            }
            if let Some(price) = data.get("price").and_then(Value::as_f64) {
                println!("PRICE: {}", price);
                self.price = price;
            }
            let fields = ["amount", "percent"];
            fields.iter().for_each(|field| {
            if let Some(changes) = data.get("changes")
                .and_then(|x| x.get("24h"))
                .and_then(|n| n.get(field))
                .and_then(Value::as_f64) {
                    let out;
                    if *field == "percent" {
                        out = format!("Change: {}%", changes);
                        self.change_percent = changes;
                    } else {
                        out = format!("Change: ${}", changes);
                        self.change_ammount = changes;
                    }
                    println!("{}", out);
                }
            })
            
        }
        Ok(())
    }

}
