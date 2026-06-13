//! Test [`hypercore_handshake::Cipher`] against JavaScript.
//! Note: where tests are named "rust_initiator_..." then JavaScript is the responder (and rust is
//! the initiator) and vice versa. Likewise where we say '..._rust_tx_first'.
mod common;

use futures::SinkExt;
use futures_lite::StreamExt;
use hypercore_handshake::{Cipher, CipherEvent, Error, state_machine::SecStream};
use tokio::{join, net::TcpListener};
use tokio_util::compat::TokioAsyncReadCompatExt;
use uint24le_framing::Uint24LELengthPrefixedFraming;

use rusty_nodejs_repl::{Config, Repl};

use common::{
    LOOPBACK, Result,
    js::{REQUIRE_JS, path_to_node_modules},
};

fn js_list_from_bytes(b: &[u8]) -> String {
    let out_str = b
        .iter()
        .map(|x| x.to_string())
        .collect::<Vec<String>>()
        .join(", ");
    format!("[{out_str}]")
}

async fn setup_rust_initiator() -> Result<(Repl, Cipher)> {
    let _ = &*REQUIRE_JS;
    let kp = hypercore_handshake::state_machine::hc_specific::generate_keypair()?;
    let pub_key_str = js_list_from_bytes(&kp.public);
    let full_secret: Vec<u8> = [&kp.private[..], &kp.public[..]].concat();
    let sec_key_str = js_list_from_bytes(&full_secret);

    let listener = TcpListener::bind(format!("{}:0", LOOPBACK)).await?;
    let port = listener.local_addr()?.port();
    let hostname = LOOPBACK;

    let setup_rs = async move {
        let tcp = listener.accept().await?.0;

        // Setup Cipher here
        let framed = Uint24LELengthPrefixedFraming::new(tcp.compat());

        let init = SecStream::new_initiator_ik(&kp.public.try_into().unwrap(), &[])?;
        let cipher = Cipher::new_init(Box::new(framed), init);

        Ok::<_, Error>(cipher)
    };

    let setup_js = async move {
        let mut conf = Config::build()?;
        conf.imports.push(
            "
NoiseSecretStream = require('@hyperswarm/secret-stream');
net = require('net');
    "
            .to_string(),
        );
        conf.path_to_node_modules = Some(path_to_node_modules()?.display().to_string());
        let mut repl = conf.start().await?;
        repl.run(format!(
            "
console.log({pub_key_str});
console.log({sec_key_str});
socket = net.connect('{port}', '{hostname}');
noiseStream = new NoiseSecretStream(false, socket, {{
    pattern: 'IK',
    keyPair: {{
      publicKey: Buffer.from({pub_key_str}),
      secretKey: Buffer.from({sec_key_str}),

    }},
}});
    "
        ))
        .await?;
        Ok::<Repl, Box<dyn std::error::Error>>(repl)
    };
    let (cipher, repl) = join!(setup_rs, setup_js);
    let cipher = cipher?;
    let repl: Repl = repl?;
    Ok((repl, cipher))
}
async fn setup_js_initiator() -> Result<(Repl, Cipher)> {
    let _ = &*REQUIRE_JS;
    let kp = hypercore_handshake::state_machine::hc_specific::generate_keypair()?;
    let pub_key_str = js_list_from_bytes(&kp.public);

    let listener = TcpListener::bind(format!("{}:0", LOOPBACK)).await?;
    let port = listener.local_addr()?.port();
    let hostname = LOOPBACK;

    let setup_rs = async move {
        let tcp = listener.accept().await?.0;

        // Setup Cipher here
        let framed = Uint24LELengthPrefixedFraming::new(tcp.compat());
        let resp = SecStream::new_responder_ik(&kp, &[])?;
        let cipher = Cipher::new_resp(Box::new(framed), resp);
        Ok::<_, Error>(cipher)
    };

    let setup_js = async move {
        let mut conf = Config::build()?;
        conf.imports.push(
            "
NoiseSecretStream = require('@hyperswarm/secret-stream');
net = require('net');
    "
            .to_string(),
        );
        conf.path_to_node_modules = Some(path_to_node_modules()?.display().to_string());
        let mut repl = conf.start().await?;
        repl.run(format!(
            "
socket = net.connect('{port}', '{hostname}');
noiseStream = new NoiseSecretStream(true, socket, {{
    pattern: 'IK',
    remotePublicKey: Buffer.from({pub_key_str}, )
}});
    "
        ))
        .await?;
        Ok::<Repl, Box<dyn std::error::Error>>(repl)
    };
    let (cipher, repl) = join!(setup_rs, setup_js);
    let cipher = cipher?;
    let repl: Repl = repl?;
    Ok((repl, cipher))
}
#[tokio::test]
async fn rust_initiator_js_tx_first() -> Result<()> {
    let (mut repl, mut cipher) = setup_rust_initiator().await?;
    repl.run(
        "
console.log('ran');
noiseStream.on('data', (data) => {{

    console.log('got data!');
    console.log(data);
}})
noiseStream.write(Buffer.from('ccc'));
",
    )
    .await?;
    let x = cipher.next().await.unwrap();
    assert!(matches!(x, CipherEvent::HandshakePayload(_)));
    let CipherEvent::Message(msg) = cipher.next().await.unwrap() else {
        panic!();
    };
    assert_eq!(msg, b"ccc");
    Ok(())
}

#[tokio::test]
async fn rust_initiator_rs_tx_first() -> Result<()> {
    let (mut repl, mut cipher) = setup_rust_initiator().await?;
    cipher.send(b"hello from rust".to_vec()).await?;
    repl.run(
        "
js_rx_msg = Deferred();
noiseStream.on('data', (data) => {{
    console.log('got data!', data);
    js_rx_msg.resolve([...data]);
}})
",
    )
    .await?;
    let js_rx_msg: Vec<u8> = repl.get_name("js_rx_msg").await?;
    assert_eq!(js_rx_msg, b"hello from rust");
    repl.run(
        "
noiseStream.write(Buffer.from('hello from js'));
",
    )
    .await?;
    let x = cipher.next().await.unwrap();
    assert!(matches!(x, CipherEvent::HandshakePayload(_)));
    let CipherEvent::Message(msg) = cipher.next().await.unwrap() else {
        panic!()
    };
    assert_eq!(msg, b"hello from js");
    Ok(())
}

#[tokio::test]
async fn js_initiator_js_tx_first() -> Result<()> {
    let (mut repl, mut cipher) = setup_js_initiator().await?;

    let rs = async move {
        let x = cipher.next().await.unwrap();
        assert!(matches!(x, CipherEvent::HandshakePayload(_)));

        let CipherEvent::Message(msg) = cipher.next().await.unwrap() else {
            panic!();
        };
        assert_eq!(msg, b"aaaa");
        cipher.send(b"zzzz".to_vec()).await?;
        Ok::<_, Error>(cipher)
    };
    let js = async move {
        let _ = repl
            .run(
                "

js_rx_first_msg = Deferred();
datas = []
noiseStream.on('data', (data) => {{
    js_rx_first_msg.resolve([...data]);
    datas.push(data);
}})
// js sends first message
noiseStream.write(Buffer.from('aaaa'));
",
            )
            .await?;

        let js_rx_first_msg: Vec<u8> = repl.get_name("js_rx_first_msg").await?;
        assert_eq!(js_rx_first_msg, b"zzzz");
        Ok::<Repl, Box<dyn std::error::Error>>(repl)
    };
    let (cipher, repl) = join!(rs, js);
    cipher?;
    repl?;
    Ok(())
}

#[tokio::test]
async fn js_initiator_rs_tx_first() -> Result<()> {
    let (mut repl, mut cipher) = setup_js_initiator().await?;

    let rs = async move {
        // TODO FIXME why do I have to listen for the HandshakePayload before sending?
        let x = cipher.next().await.unwrap();
        assert!(matches!(x, CipherEvent::HandshakePayload(_)));

        cipher.send(b"zzzz".to_vec()).await?;
        let CipherEvent::Message(msg) = cipher.next().await.unwrap() else {
            panic!();
        };
        assert_eq!(msg, b"aaa");
        Ok::<_, Error>(cipher)
    };

    let js = async move {
        let _ = repl
            .run(
                "

js_rx_first_msg = Deferred();
datas = []
noiseStream.on('data', (data) => {{
    js_rx_first_msg.resolve([...data]);
    datas.push(data);
}})
// js wait to send msg
",
            )
            .await?;

        let js_rx_first_msg: Vec<u8> = repl.get_name("js_rx_first_msg").await?;
        assert_eq!(js_rx_first_msg, b"zzzz");
        repl.run(
            "
noiseStream.write(Buffer.from('aaa'));
        ",
        )
        .await?;
        Ok::<Repl, Box<dyn std::error::Error>>(repl)
    };

    let (cipher, repl) = join!(rs, js);
    cipher?;
    repl?;
    Ok(())
}
