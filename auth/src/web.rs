//! Cómo se responde: las páginas que ve una persona, las respuestas JSON de la
//! web y las cabeceras de CORS.

use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::middleware;
use serde_json::Value;

pub(crate) fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub(crate) fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html lang=\"es\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>{t} · Mestisexo</title><style>body{{background:#0e0e10;color:#efeff1;font:16px system-ui,sans-serif;\
         max-width:520px;margin:15vh auto;padding:0 20px}}a{{color:#a970ff}}</style></head>\
         <body><h1>{t}</h1>{body}</body></html>",
        t = esc(title)
    ))
}

pub(crate) fn api(status: StatusCode, v: Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], axum::Json(v)).into_response()
}

/// No se usan cookies, solo el token en `Authorization`, así que se permite
/// cualquier origen.
pub(crate) async fn cors(req: axum::extract::Request, next: middleware::Next) -> Response {
    let mut res = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("authorization, content-type"));
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST"));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    res
}

pub(crate) async fn paid() -> Html<String> {
    page("Gracias", "<p>Si el pago se completó, tu insignia de suscriptor aparecerá en el chat en unos segundos. Ya puedes cerrar esta pestaña.</p><p><a href=\"/\">Volver al directo</a></p>")
}
