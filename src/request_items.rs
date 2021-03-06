use reqwest::blocking::multipart;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, CONNECTION, HOST,
};

use crate::{RequestItem, Url};

pub struct RequestItems(Vec<RequestItem>);

pub enum Body {
    Json(serde_json::Map<String, serde_json::Value>),
    Form(Vec<(String, String)>),
    Multipart(multipart::Form),
}

impl RequestItems {
    pub fn new(request_items: Vec<RequestItem>) -> RequestItems {
        RequestItems(request_items)
    }

    pub fn headers(&self, url: &Url) -> HeaderMap<HeaderValue> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip, deflate"));
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert(HOST, HeaderValue::from_str(&url.host().unwrap()).unwrap());
        for item in &self.0 {
            match item {
                RequestItem::HttpHeader(key, value) => {
                    let key = HeaderName::from_bytes(&key.as_bytes()).unwrap();
                    let value = HeaderValue::from_str(&value).unwrap();
                    headers.insert(key, value);
                }
                _ => {}
            }
        }
        headers
    }

    pub fn query(&self) -> Vec<(&String, &String)> {
        let mut query = vec![];
        for item in &self.0 {
            match item {
                RequestItem::UrlParam(key, value) => {
                    query.push((key, value));
                }
                _ => {}
            }
        }
        query
    }

    pub fn body(&self, as_form: bool) -> Result<Option<Body>, &str> {
        if !as_form {
            let mut body = serde_json::Map::new();
            for item in &self.0 {
                match item.clone() {
                    RequestItem::JSONField(key, value) => {
                        body.insert(key, value);
                    }
                    RequestItem::DataField(key, value) => {
                        body.insert(key, serde_json::Value::String(value));
                    }
                    RequestItem::FormFile(_, _) => {
                        return Err("Sending Files is not supported when the request body is in JSON format");
                    }
                    _ => {}
                }
            }
            if body.len() > 0 {
                Ok(Some(Body::Json(body)))
            } else {
                Ok(None)
            }
        } else {
            let mut text_fields = Vec::<(String, String)>::new();
            let mut files = Vec::<(String, String)>::new();
            for item in &self.0 {
                match item.clone() {
                    RequestItem::JSONField(_, _) => {
                        return Err("JSON values are not supported in Form fields");
                    }
                    RequestItem::DataField(key, value) => text_fields.push((key, value)),
                    RequestItem::FormFile(key, value) => files.push((key, value)),
                    _ => {}
                }
            }
            match (text_fields.len(), files.len()) {
                (0, 0) => Ok(None),
                (_, 0) => Ok(Some(Body::Form(text_fields))),
                (_, _) => {
                    let mut form = multipart::Form::new();
                    for (key, value) in text_fields {
                        form = form.text(key, value);
                    }
                    for (key, value) in files {
                        form = form.file(key, value).unwrap();
                    }
                    Ok(Some(Body::Multipart(form)))
                }
            }
        }
    }
}
