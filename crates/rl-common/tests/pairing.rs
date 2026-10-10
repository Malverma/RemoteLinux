use rl_common::identity::Identity;
use rl_common::pairing::{pair_as_client, pair_as_host, HostPairOutcome};
use rl_common::transport::{self, ALPN_PAIR};

async fn run(host_pin: Option<&str>, client_pin: &str) -> (anyhow::Result<rl_common::pairing::Paired>, HostPairOutcome, String, String) {
    let host_id = Identity::generate().unwrap();
    let client_id = Identity::generate().unwrap();
    let server = transport::server_endpoint(&host_id, "127.0.0.1:0".parse().unwrap()).unwrap();
    let addr = server.local_addr().unwrap();
    let host_fp = host_id.fingerprint.clone();
    let host_pin = host_pin.map(str::to_string);
    let host_task = tokio::spawn(async move {
        let conn = server.accept().await.unwrap().await.unwrap();
        assert_eq!(transport::alpn(&conn).as_deref(), Some(ALPN_PAIR));
        pair_as_host(&conn, &host_fp, "desk", host_pin.as_deref()).await.unwrap()
    });
    let client = transport::client_endpoint().unwrap();
    let conn = client
        .connect_with(transport::client_config(&client_id, ALPN_PAIR).unwrap(), addr, "remotelinux")
        .unwrap()
        .await
        .unwrap();
    let res = pair_as_client(&conn, &client_id.fingerprint, "laptop", client_pin).await;
    let host_res = host_task.await.unwrap();
    (res, host_res, host_id.fingerprint, client_id.fingerprint)
}

#[tokio::test]
async fn correct_pin_pairs_and_exchanges_fingerprints() {
    let (res, host, host_fp, client_fp) = run(Some("123456"), "123456").await;
    let p = res.unwrap();
    assert_eq!(p.peer_name, "desk");
    assert_eq!(p.peer_fingerprint, host_fp);
    match host {
        HostPairOutcome::Paired(c) => {
            assert_eq!(c.peer_name, "laptop");
            assert_eq!(c.peer_fingerprint, client_fp);
        }
        _ => panic!("host did not pair"),
    }
}

#[tokio::test]
async fn wrong_pin_fails() {
    let (res, host, _, _) = run(Some("123456"), "654321").await;
    assert!(res.unwrap_err().to_string().contains("wrong PIN"));
    assert!(matches!(host, HostPairOutcome::WrongPin));
}

#[tokio::test]
async fn not_in_pairing_mode() {
    let (res, host, _, _) = run(None, "123456").await;
    assert!(res.unwrap_err().to_string().contains("not in pairing mode"));
    assert!(matches!(host, HostPairOutcome::Refused(_)));
}
