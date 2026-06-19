//! HTTP API + статический `WebUI` для coordination-сервера (Фаза 3).
//!
//! Биндится на отдельный порт от signaling/relay. По умолчанию `127.0.0.1`
//! (localhost-only) — админка не торчит наружу без явного флага `--web-expose`.
//! Запрос с не-localhost без флага → 403, не молчаливое выставление.
//!
//! REST:
//! - `GET /` — статический HTML+JS `WebUI` (без сборочного пайплайна).
//! - `GET /api/networks` — список сетей с пирами (read-only snapshot).
//! - `POST /api/kick` — `{network_id, peer_id, reason}` → кик пира.
//! - `POST /api/close` — `{network_id, reason}` → закрыть сеть.
//!
//! Аутентификации/мультитенантности нет осознанно (это не публичный `SaaS`);
//! localhost-bind + optional expose — достаточная граница для PoC/MVP.

use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::thread;

use serde::Deserialize;

use crate::http::{self, Method, Request, Response};
use crate::registry::Registry;

/// Запустить HTTP-сервер. Блокирующий; для main — `thread::spawn`. Ошибка
/// `accept` логируется и не роняет цикл. `R: Clone` — копия на каждый запрос.
///
/// `listener` передаётся owned, а не `&TcpListener`, потому что `serve`
/// запускается в отдельном `'static`-потоке и владеет сокетом; заимствовать
/// его у caller'а нельзя (caller не переживает spawn). `TcpListener::accept`
/// берёт `&self`, но ресурс должен жить вместе с потоком — это законный owned.
#[allow(clippy::needless_pass_by_value)]
pub fn serve<R: Registry + Clone + 'static>(listener: TcpListener, registry: &R, web_expose: bool) {
    log::info!(
        "web listening on {} ({})",
        listener
            .local_addr()
            .map_or_else(|_| "<unknown>".to_string(), |a| a.to_string()),
        if web_expose { "exposed" } else { "localhost-only" }
    );
    loop {
        match listener.accept() {
            Ok((stream, peer)) => {
                let registry = registry.clone();
                if let Err(e) = thread::Builder::new()
                    .name("web".into())
                    .spawn(move || handle_connection(stream, peer, &registry, web_expose))
                {
                    log::error!("web: failed to spawn handler: {e}");
                }
            }
            Err(e) => log::warn!("web: accept failed: {e}"),
        }
    }
}

fn handle_connection<R: Registry + 'static>(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: &R,
    web_expose: bool,
) {
    // localhost-only gate: запрос с не-localhost без флага → 403, не молчаливо.
    if !web_expose && !is_localhost(&peer) {
        let _ = http::write_response(&mut stream, &Response::text(403, "403 Forbidden: web admin is localhost-only; pass --web-expose to bind 0.0.0.0"));
        return;
    }

    let req = match http::read_request(&mut stream) {
        Ok(Some(r)) => r,
        Ok(None) => return, // чистый EOF.
        Err(e) => {
            log::debug!("web {peer}: bad request: {e}");
            let _ = http::write_response(&mut stream, &Response::text(400, &format!("400 Bad Request: {e}")));
            return;
        }
    };

    let resp = route(&req, &registry);
    if let Err(e) = http::write_response(&mut stream, &resp) {
        log::debug!("web {peer}: write response failed: {e}");
    }
}

fn is_localhost(addr: &SocketAddr) -> bool {
    matches!(addr.ip(), IpAddr::V4(v4) if v4.is_loopback()) || matches!(addr.ip(), IpAddr::V6(v6) if v6.is_loopback())
}

fn route<R: Registry + ?Sized>(req: &Request, registry: &R) -> Response {
    match (req.method, req.path.as_str()) {
        (Method::Get, "/") => Response::html(200, WEBUI_HTML.as_bytes().to_vec()),
        (Method::Get, "/api/networks") => api_networks(registry),
        (Method::Post, "/api/kick") => api_kick(req, registry),
        (Method::Post, "/api/close") => api_close(req, registry),
        (_, path) if path.starts_with("/api/") => {
            Response::text(404, "404 Not Found: unknown API endpoint")
        }
        (_, _) => Response::text(404, "404 Not Found"),
    }
}

fn api_networks<R: Registry + ?Sized>(registry: &R) -> Response {
    let snapshot = registry.snapshot();
    match serde_json::to_vec(&snapshot) {
        Ok(body) => Response::json(200, body),
        Err(e) => {
            log::error!("web: serialize snapshot failed: {e}");
            Response::text(500, "500 Internal Server Error: serialize failed")
        }
    }
}

#[derive(Deserialize)]
struct KickRequest {
    network_id: String,
    peer_id: String,
    #[serde(default)]
    reason: String,
}

fn api_kick<R: Registry + ?Sized>(req: &Request, registry: &R) -> Response {
    let parsed: KickRequest = match serde_json::from_slice(&req.body) {
        Ok(p) => p,
        Err(e) => return Response::text(400, &format!("400 Bad Request: {e}")),
    };
    let net = match lattice_proto::NetworkId::from_hex(&parsed.network_id) {
        Ok(n) => n,
        Err(e) => return Response::text(400, &format!("400 Bad Request: {e}")),
    };
    let peer = lattice_proto::PeerId::new(parsed.peer_id);
    registry.kick(&net, &peer, &parsed.reason);
    Response::text(204, "")
}

#[derive(Deserialize)]
struct CloseRequest {
    network_id: String,
    #[serde(default)]
    reason: String,
}

fn api_close<R: Registry + ?Sized>(req: &Request, registry: &R) -> Response {
    let parsed: CloseRequest = match serde_json::from_slice(&req.body) {
        Ok(p) => p,
        Err(e) => return Response::text(400, &format!("400 Bad Request: {e}")),
    };
    let net = match lattice_proto::NetworkId::from_hex(&parsed.network_id) {
        Ok(n) => n,
        Err(e) => return Response::text(400, &format!("400 Bad Request: {e}")),
    };
    registry.close_network(&net, &parsed.reason);
    Response::text(204, "")
}

/// Минимальный статический `WebUI`: HTML+JS без сборочного пайплайна. JS
/// опрашивает `/api/networks` каждые 3с и рендерит таблицы; кнопки kick/close
/// шлют POST. Ссылок на внешние ресурсы нет — всё inline, работает без интернета.
const WEBUI_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Lattice — Coordination</title>
<style>
body{font:14px system-ui,Segoe UI,sans-serif;margin:2rem;background:#fafafa;color:#222}
h1{margin:0 0 .5rem}
table{border-collapse:collapse;margin:1rem 0;width:100%}
th,td{border:1px solid #ddd;padding:.35rem .5rem;text-align:left;font-size:13px}
th{background:#eee}
.peer-online{color:#080;font-weight:600}.peer-degraded{color:#a60}.peer-offline{color:#c00}
.link-Direct{color:#080}.link-Relay{color:#a60}.link-Unknown{color:#888}
button{font:inherit;cursor:pointer;border:1px solid #888;background:#fff;padding:.2rem .6rem;border-radius:3px}
button:hover{background:#eee}
.muted{color:#666}
</style></head><body>
<h1>Lattice — Coordination</h1>
<p class="muted">read-only обзор сетей и пиров; refresh каждые 3с.</p>
<div id="networks"></div>
<script>
async function load(){
  try{
    const r = await fetch('/api/networks');
    if(!r.ok){ document.getElementById('networks').innerHTML = '<p>HTTP '+r.status+'</p>'; return; }
    const nets = await r.json();
    const root = document.getElementById('networks');
    if(!nets.length){ root.innerHTML = '<p class=muted>нет активных сетей.</p>'; return; }
    root.innerHTML = nets.map(function(n){
      const rows = n.peers.map(function(p){
        const links = p.links.map(function(l){
          // /api/networks сериализует links как кортежи [to, kind]
          // (PeerSnapshot.links: Vec<(PeerId, LinkKind)>), не объекты.
          const to = l[0], kind = l[1];
          return '<span class="link-'+kind+'">'+kind+'→'+to+'</span>';
        }).join(' ');
        return '<tr><td>'+p.peer_id+'</td><td>'+p.overlay_ip+'</td><td>'+p.srflx+
               '</td><td>'+p.nat+'</td><td class="peer-'+p.status+'">'+p.status+
               '</td><td>'+links+'</td><td>'+
               '<button onclick="kick(\''+n.network_id+'\',\''+p.peer_id+'\')">kick</button></td></tr>';
      }).join('');
      return '<h2>net '+n.network_id.slice(0,12)+'… <span class=muted>(relay #'+n.relay_session+', '+
             n.peers.length+' peers)</span></h2>'+
             '<table><tr><th>peer</th><th>overlay</th><th>srflx</th><th>NAT</th>'+
             '<th>status</th><th>links</th><th></th></tr>'+rows+'</table>'+
             '<button onclick="closeNet(\''+n.network_id+'\')">close network</button>';
    }).join('');
  }catch(e){ document.getElementById('networks').innerHTML = '<p>error: '+e+'</p>'; }
}
async function kick(net,peer){
  if(!confirm('kick peer '+peer+' from network?')) return;
  await fetch('/api/kick',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({network_id:net,peer_id:peer,reason:'admin kick'})});
  load();
}
async function closeNet(net){
  if(!confirm('close network '+net.slice(0,12)+'… ? all peers will get NetworkClosed.')) return;
  await fetch('/api/close',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({network_id:net,reason:'admin close'})});
  load();
}
load();
setInterval(load, 3000);
</script></body></html>"#;
