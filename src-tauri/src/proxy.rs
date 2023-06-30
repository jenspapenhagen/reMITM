use std::{fs, sync::Mutex};
use std::net::SocketAddr;

use once_cell::sync::Lazy;

use hudsucker::{
  async_trait::async_trait,
  certificate_authority::RcgenAuthority,
  hyper::{Body, Request, Response, StatusCode},
  *,
};

use rustls_pemfile as pemfile;

#[cfg(windows)]
use registry::{Data, Hive, Security};

use crate::config::default_config;
use crate::{config, certificate};

// Globally store the server we are redirecting to
static REDIRECT_TO: Lazy<Mutex<String>> = Lazy::new(|| Mutex::new(default_config().redirect_to.unwrap()));

async fn shutdown_signal() {
  tokio::signal::ctrl_c()
    .await
    .expect("Failed to install CTRL+C signal handler");
}

#[derive(Clone)]
pub struct ProxyHandler;

#[async_trait]
impl HttpHandler for ProxyHandler {
  async fn should_intercept(&mut self, _ctx: &HttpContext, req: &Request<Body>) -> bool {
    // Get URIs from config
    let config = config::get_config();
    let urls_to_redirect = config.urls_to_redirect.unwrap_or_else(|| default_config().urls_to_redirect.unwrap());

    // Get the request URI
    let uri = req.uri().clone();

    // Check if the request URI matches any of the URIs in the config
    for url in urls_to_redirect {
      if url == uri.to_string() {
        return true;
      }
    }

    false
  }
    
  async fn handle_request(
    &mut self,
    _ctx: &HttpContext,
    mut req: Request<Body>,
  ) -> RequestOrResponse {
    // Handle CONNECT
    if req.method().as_str() == "CONNECT" {
      let builder = Response::builder()
        .header("DecryptEndpoint", "Created")
        .status(StatusCode::OK);

      let res = builder.body(()).unwrap();

      *res.body()
    }

    // Get URIs from config
    let config = config::get_config();
    let urls_to_redirect = config.urls_to_redirect.unwrap_or_else(|| default_config().urls_to_redirect.unwrap());
    
    // Get the request URI
    let uri = req.uri().clone();
    let mut do_redirect = false;
    
    // Check if the request URI matches any of the URIs in the config
    for url in urls_to_redirect {
      println!("Comparing {} with {}", url, uri);

      if uri.to_string().contains(&url) {
        do_redirect = true;
      }
    }

    // If we don't need to redirect, just let the request continue
    if !do_redirect {
      return req.into();
    }

    println!("Found URI to redirect: {}", req.uri());

    let path_and_query = req.uri().path_and_query();
    let mut new_uri = format!("{}", REDIRECT_TO.lock().unwrap());

    if path_and_query.is_some() {
      new_uri = format!("{}{}", new_uri, path_and_query.unwrap());
    }

    println!("Redirecting to {}...", new_uri);

    *req.uri_mut() = new_uri.parse().unwrap();

    req.into()
  }

  async fn handle_response(
    &mut self,
    _context: &HttpContext,
    response: Response<Body>,
  ) -> Response<Body> {
    response
  }
}

#[tauri::command]
pub fn set_redirect_server(server: String) {
  *REDIRECT_TO.lock().unwrap() = server.clone();

  // Set in config
  let mut config = config::get_config();
  config.redirect_to = Some(server);
  config::write_config(config);  

  println!("Redirecting requests to: {}", REDIRECT_TO.lock().unwrap());
}

/**
 * Starts the HTTP(S) proxy server.
 */
pub async fn create_proxy() {
  let proxy_port = config::get_config().proxy_port.unwrap_or_else(|| default_config().proxy_port.unwrap());
  let certificate_path = certificate::cert_path();

  let cert_path = certificate_path;
  let pk_path = cert_path.join("private.key");
  let ca_path = cert_path.join("cert.crt");

  // Get the certificate and private key.
  let mut private_key_bytes: &[u8] = &fs::read(pk_path).expect("Could not read private key");
  let mut ca_cert_bytes: &[u8] = &fs::read(ca_path).expect("Could not read certificate");

  // Parse the private key and certificate.
  let private_key = rustls::PrivateKey(
    pemfile::pkcs8_private_keys(&mut private_key_bytes)
      .expect("Failed to parse private key")
      .remove(0),
  );

  let ca_cert = rustls::Certificate(
    pemfile::certs(&mut ca_cert_bytes)
      .expect("Failed to parse CA certificate")
      .remove(0),
  );

  // Create the certificate authority.
  let authority = RcgenAuthority::new(private_key, ca_cert, 1_000)
    .expect("Failed to create Certificate Authority");

  // Create an instance of the proxy.
  let proxy = ProxyBuilder::new()
    .with_addr(SocketAddr::from(([0, 0, 0, 0], proxy_port)))
    .with_rustls_client()
    .with_ca(authority)
    .with_http_handler(ProxyHandler)
    .build();

  // Start the proxy.
  tokio::spawn(proxy.start(shutdown_signal()));
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn connect_to_proxy() {
  // Create the server string
  let config = config::get_config();
  let proxy_port = config.proxy_port.unwrap_or_else(|| default_config().proxy_port.unwrap());
  let server = format!("http=127.0.0.1:{};https=127.0.0.1:{}", proxy_port, proxy_port);

  // Fetch the 'Internet Settings' registry key.
  let settings = Hive::CurrentUser
    .open(
      r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
      Security::AllAccess,
    )
    .unwrap();

  // Set registry values.
  settings
    .set_value("ProxyServer", &Data::String(server.parse().unwrap()))
    .unwrap();
  settings.set_value("ProxyEnable", &Data::U32(1)).unwrap();

  println!("Connected to the proxy.");
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn connect_to_proxy() {
  use std::process::Command;

  // Create the server string
  let config = config::get_config();
  let proxy_port = config.proxy_port.unwrap_or_else(|| default_config().proxy_port.unwrap());
  let server = format!("127.0.0.1");

  // Set the proxy via gsettings
  let set_proxy = Command::new("gsettings")
    .arg("set")
    .arg("org.gnome.system.proxy.http")
    .arg("host")
    .arg(server.clone())
    .output()
    .expect("failed to execute process");

  println!("Set http proxy: {}", set_proxy.status);

  // Set https proxy as well
  let set_proxy = Command::new("gsettings")
    .arg("set")
    .arg("org.gnome.system.proxy.https")
    .arg("host")
    .arg(server)
    .output()
    .expect("failed to execute process");

  println!("Set https proxy: {}", set_proxy.status);

  // Set proxy mode to manual
  let set_proxy = Command::new("gsettings")
    .arg("set")
    .arg("org.gnome.system.proxy")
    .arg("mode")
    .arg("manual")
    .output()
    .expect("failed to execute process");

  println!("Set proxy mode: {}", set_proxy.status);

  if !set_proxy.status.success() {
    println!("Failed to set proxy: {}", set_proxy.status);
  }
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn connect_to_proxy() {
  use std::process::Command;

  // Create the server string
  let config = config::get_config();
  let proxy_port = format!("{}", config.proxy_port.unwrap_or_else(|| default_config().proxy_port.unwrap()));

  // Set the proxy via networksetup
  Command::new("networksetup")
    .arg("-setwebproxy")
    .arg("Wi-Fi")
    .arg("127.0.0.1")
    .arg(proxy_port.clone())
    .output()
    .expect("failed to execute process");

  Command::new("networksetup")
    .arg("-setsecurewebproxy")
    .arg("Wi-Fi")
    .arg("127.0.0.1")
    .arg(proxy_port)
    .output()
    .expect("failed to execute process");

  Command::new("networksetup")
    .arg("-setwebproxystate")
    .arg("Wi-Fi")
    .arg("on")
    .output()
    .expect("failed to execute process");
}

#[cfg(target_os = "windows")]
#[tauri::command]
pub fn disconnect_from_proxy() {
  // Fetch the 'Internet Settings' registry key.
  let settings = Hive::CurrentUser
    .open(
      r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
      Security::AllAccess,
    )
    .unwrap();

  // Set registry values.
  settings.set_value("ProxyEnable", &Data::U32(0)).unwrap();

  println!("Disconnected from the proxy.");
}

#[cfg(target_os = "linux")]
#[tauri::command]
pub fn disconnect_from_proxy() {
  use std::process::Command;

  // Set the proxy via gsettings
  let set_proxy = Command::new("gsettings")
    .arg("set")
    .arg("org.gnome.system.proxy")
    .arg("mode")
    .arg("none")
    .output()
    .expect("failed to execute process");

  if !set_proxy.status.success() {
    println!("Failed to set proxy: {}", set_proxy.status);
  }
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn disconnect_from_proxy() {
  use std::process::Command;

  // Set the proxy via networksetup
  let set_proxy = Command::new("networksetup")
    .arg("-setwebproxystate")
    .arg("Wi-Fi")
    .arg("off")
    .output()
    .expect("failed to execute process");
}