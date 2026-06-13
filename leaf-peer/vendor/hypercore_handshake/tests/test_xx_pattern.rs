//! Unit test demonstrating the XX handshake pattern flow

use hypercore_handshake::state_machine::{SecStream, hc_specific};

#[test]
fn test_xx_pattern_flow() -> Result<(), Box<dyn std::error::Error>> {
    // Generate keypair for responder
    let resp_kp = hc_specific::generate_keypair()?;

    // For XX pattern, initiator doesn't need to know responder's public key
    let initiator = SecStream::new_initiator_xx(&[])?;
    let responder = SecStream::new_responder_xx(&resp_kp, &[])?;

    println!("=== XX Pattern Handshake Flow ===");

    // Message 1: Initiator → Responder (e)
    println!("\n1. Initiator sends first message (ephemeral key only)");
    let (initiator, msg0) = initiator.write_msg(Some(b"init_payload"))?;
    println!("   Message 1 length: {} bytes", msg0.len());

    // Message 2: Responder receives and responds (e, ee, s, es)
    let (responder, init_payload) = responder.read_msg(&msg0)?;
    assert_eq!(init_payload, b"init_payload");

    // Responder sends response (handshake message, setup is empty for XX at this point)
    println!("\n2. Responder sends first message");
    let (responder, [msg1_hs, _msg_2_empty]) = responder.write_msg(Some(b"resp_payload"))?;
    assert!(_msg_2_empty.is_empty());
    println!("  Responder sends handshake message:");
    println!("     - Handshake message: {} bytes", msg1_hs.len());
    println!("     - (Setup deferred until after third handshake message)");

    let (initiator, resp_payload) = initiator.read_msg(&msg1_hs)?;
    assert_eq!(resp_payload, b"resp_payload");

    // For XX: Initiator needs to send third handshake message (s, se)
    println!("\n3. Initiator EncReady. sends final handshake message (static key)");
    let (initiator, msg3_third) = initiator.write_msg()?;
    // Responder receives third message, handshake now complete
    let (responder, no_payload) = responder.read_msg(&msg3_third)?;
    assert!(no_payload.is_empty());

    println!("Both sides can encrypt, but must receive decryptor message next");
    let (responder, msg4) = responder.write_msg()?;

    // Now both sides send setup messages
    println!("\n6.  send setup messages");
    let (initiator, init_setup) = initiator.write_msg()?;
    println!("   Initiator setup message: {} bytes", init_setup.len());

    // Complete the handshake on both sides
    println!("\n7. Both sides finalize the connection");
    let mut initiator = initiator.read_msg(&msg4)?;
    let mut responder = responder.read_msg(&init_setup)?;

    // Verify handshake hashes match (proves both sides completed the same handshake)
    let init_hash = initiator.handshake_hash();
    let resp_hash = responder.handshake_hash();
    assert_eq!(init_hash, resp_hash, "Handshake hashes must match");
    println!("   Handshake hash matches: {:02x?}...", &init_hash[..8]);

    // Verify both sides can encrypt/decrypt messages
    println!("\n7. Test encryption/decryption");
    let mut msg = b"Hello from initiator!".to_vec();
    initiator.push(&mut msg, &[], crypto_secretstream::Tag::Message)?;
    println!("   Encrypted message length: {} bytes", msg.len());

    let tag = responder.pull(&mut msg, &[])?;
    assert_eq!(msg, b"Hello from initiator!");
    assert_eq!(tag, crypto_secretstream::Tag::Message);
    println!("   Decrypted message: {:?}", String::from_utf8_lossy(&msg));

    println!("\n=== XX Pattern Handshake Complete! ===");
    Ok(())
}

#[test]
fn test_comparison_ik_vs_xx() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n=== Comparing IK vs XX Patterns ===\n");

    let _init_kp = hc_specific::generate_keypair()?;
    let resp_kp = hc_specific::generate_keypair()?;

    // IK Pattern
    println!("IK Pattern:");
    let resp_pubkey: [u8; 32] = resp_kp.public.clone().try_into().unwrap();
    let ik_init = SecStream::new_initiator_ik(&resp_pubkey, &[])?;
    let ik_resp = SecStream::new_responder_ik(&resp_kp, &[])?;

    let (ik_init, msg1) = ik_init.write_msg(None)?;
    println!(
        "  - Initiator msg 1: {} bytes (includes static key)",
        msg1.len()
    );

    let (_ik_resp, _) = ik_resp.read_msg(&msg1)?;
    let (_ik_resp, [msg2_hs, msg2_setup]) = _ik_resp.write_msg(None)?;
    println!(
        "  - Responder msg 2: {} bytes (handshake) + {} bytes (setup)",
        msg2_hs.len(),
        msg2_setup.len()
    );

    let (ik_init, _) = ik_init.read_msg(&msg2_hs)?;
    let (_ik_init, msg3) = ik_init.write_msg()?;
    println!("  - Initiator msg 3: {} bytes (setup only)", msg3.len());
    println!("  - Total: 3 handshake messages\n");

    // XX Pattern
    println!("XX Pattern:");
    let xx_init = SecStream::new_initiator_xx(&[])?;
    let xx_resp = SecStream::new_responder_xx(&resp_kp, &[])?;

    let (xx_init, msg1) = xx_init.write_msg(None)?;
    println!("  - Initiator msg 1: {} bytes (ephemeral only)", msg1.len());

    let (xx_resp, _) = xx_resp.read_msg(&msg1)?;
    let (xx_resp, [msg2_hs, _msg2_empty]) = xx_resp.write_msg(None)?;
    println!(
        "  - Responder msg 2: {} bytes (handshake, setup deferred)",
        msg2_hs.len()
    );

    let (xx_init, _) = xx_init.read_msg(&msg2_hs)?;
    let (xx_init, msg3_third) = xx_init.write_msg()?;
    println!(
        "  - Initiator msg 3: {} bytes (static key)",
        msg3_third.len()
    );

    let (xx_resp, _) = xx_resp.read_msg(&msg3_third)?;
    let (_xx_init, init_setup) = xx_init.write_msg()?;
    let (_xx_resp, resp_setup) = xx_resp.write_msg()?;
    println!("  - Initiator msg 4: {} bytes (setup)", init_setup.len());
    println!("  - Responder msg 4: {} bytes (setup)", resp_setup.len());
    println!("  - Total: 4 handshake messages (3 noise + 2 setup)\n");

    println!("Key Difference:");
    println!("  - IK: Initiator knows responder's key upfront, sends it in first message");
    println!("  - XX: Neither knows the other's key, exchange them during handshake");

    Ok(())
}
