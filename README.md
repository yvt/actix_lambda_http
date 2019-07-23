# actix_lambda_http

[<img src="https://docs.rs/actix_lambda_http/badge.svg" alt="docs.rs">](https://docs.rs/actix_lambda_http/)

[Actix]-[AWS Lambda] connector for Actix 1.x

[Actix]: https://crates.io/crates/actix-web
[AWS Lambda]: https://crates.io/crates/lambda_http

This crate provides an AWS Lambda handler function that responds to ALB and
API Gateway proxy events using a provided Actix web application.

## Usage

```rust
use actix_web::{App, HttpResponse, web};

fn index(req: actix_web::HttpRequest) -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain")
        .body(format!("request data:\n\n{:#?}", req))
}

fn main() {
    actix_lambda_http::LambdaHttpServer::new(|| {
        App::new()
            .wrap(actix_web::middleware::Logger::default())
            .route("/", web::to(index))
    })
    .binary_media_types(vec!["image/png"])
    .start()
    .unwrap();
}
```

License: MIT/Apache-2.0
