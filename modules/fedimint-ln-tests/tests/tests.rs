use std::str::FromStr;
use std::time::Duration;

use assert_matches::assert_matches;
use bitcoin::secp256k1::rand::thread_rng;
use bitcoin::{secp256k1, KeyPair};
use fedimint_client::Client;
use fedimint_core::task::sleep;
use fedimint_core::util::NextOrPending;
use fedimint_core::{sats, Amount};
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_common::config::DummyGenParams;
use fedimint_dummy_server::DummyInit;
use fedimint_ln_client::{
    InternalPayState, LightningClientInit, LightningClientModule, LightningOperationMeta,
    LnPayState, LnReceiveState, OutgoingLightningPayment, PayType,
};
use fedimint_ln_common::config::LightningGenParams;
use fedimint_ln_common::ln_operation;
use fedimint_ln_server::LightningInit;
use fedimint_testing::federation::FederationTest;
use fedimint_testing::fixtures::Fixtures;
use fedimint_testing::gateway::{GatewayTest, DEFAULT_GATEWAY_PASSWORD};
use lightning_invoice::Bolt11Invoice;
use tracing::info;

fn fixtures() -> Fixtures {
    let fixtures = Fixtures::new_primary(DummyClientInit, DummyInit, DummyGenParams::default());
    let ln_params = LightningGenParams::regtest(fixtures.bitcoin_server());
    fixtures.with_module(LightningClientInit, LightningInit, ln_params)
}

/// Setup a gateway connected to the fed and client
async fn gateway(fixtures: &Fixtures, fed: &FederationTest) -> GatewayTest {
    let lnd = fixtures.lnd().await;
    let mut gateway = fixtures
        .new_gateway(lnd, 0, Some(DEFAULT_GATEWAY_PASSWORD.to_string()))
        .await;
    gateway.connect_fed(fed).await;
    gateway
}

async fn pay_invoice(
    client: &Client,
    invoice: Bolt11Invoice,
) -> anyhow::Result<OutgoingLightningPayment> {
    let ln_module = client.get_first_module::<LightningClientModule>();
    let gateway = ln_module.select_active_gateway_opt().await;
    ln_module.pay_bolt11_invoice(gateway, invoice, ()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn can_switch_active_gateway() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let client = fed.new_client().await;
    let mut gateway1 = fixtures
        .new_gateway(
            fixtures.lnd().await,
            0,
            Some(DEFAULT_GATEWAY_PASSWORD.to_string()),
        )
        .await;
    let mut gateway2 = fixtures
        .new_gateway(
            fixtures.cln().await,
            0,
            Some(DEFAULT_GATEWAY_PASSWORD.to_string()),
        )
        .await;

    // Client selects a gateway by default
    gateway1.connect_fed(&fed).await;
    let key1 = gateway1.get_gateway_id();
    assert_eq!(
        client
            .get_first_module::<LightningClientModule>()
            .select_active_gateway()
            .await?
            .gateway_id,
        key1
    );

    gateway2.connect_fed(&fed).await;
    let key2 = gateway1.get_gateway_id();
    let gateways = client
        .get_first_module::<LightningClientModule>()
        .fetch_registered_gateways()
        .await
        .unwrap();
    assert_eq!(gateways.len(), 2);

    client
        .get_first_module::<LightningClientModule>()
        .set_active_gateway(&key2)
        .await?;
    assert_eq!(
        client
            .get_first_module::<LightningClientModule>()
            .select_active_gateway()
            .await?
            .gateway_id,
        key2
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_can_attach_extra_meta_to_receive_operation() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();

    // Print money for client2
    let (op, outpoint) = client2_dummy_module.print_money(sats(1000)).await?;
    client2.await_primary_module_output(op, outpoint).await?;

    let extra_meta = "internal payment with no gateway registered".to_string();
    let (op, invoice, _) = client1
        .get_first_module::<LightningClientModule>()
        .create_bolt11_invoice(
            sats(250),
            "with-markers".to_string(),
            None,
            extra_meta.clone(),
        )
        .await?;
    let mut sub1 = client1
        .get_first_module::<LightningClientModule>()
        .subscribe_ln_receive(op)
        .await?
        .into_stream();
    assert_eq!(sub1.ok().await?, LnReceiveState::Created);
    assert_matches!(sub1.ok().await?, LnReceiveState::WaitingForPayment { .. });

    // Pay the invoice from client2
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
            assert_eq!(sub1.ok().await?, LnReceiveState::Funded);
        }
        _ => panic!("Expected internal payment!"),
    }

    // Verify that we can retrieve the extra metadata that was attached
    let operation = ln_operation(&client1, op).await?;
    let op_meta = operation
        .meta::<LightningOperationMeta>()
        .extra_meta
        .to_string();
    assert_eq!(serde_json::to_string(&extra_meta)?, op_meta);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cannot_pay_same_internal_invoice_twice() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();

    // Print money for client2
    let (op, outpoint) = client2_dummy_module.print_money(sats(1000)).await?;
    client2.await_primary_module_output(op, outpoint).await?;

    // TEST internal payment when there are no gateways registered
    let (op, invoice, _) = client1
        .get_first_module::<LightningClientModule>()
        .create_bolt11_invoice(sats(250), "with-markers".to_string(), None, ())
        .await?;
    let mut sub1 = client1
        .get_first_module::<LightningClientModule>()
        .subscribe_ln_receive(op)
        .await?
        .into_stream();
    assert_eq!(sub1.ok().await?, LnReceiveState::Created);
    assert_matches!(sub1.ok().await?, LnReceiveState::WaitingForPayment { .. });

    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice.clone()).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
            assert_eq!(sub1.ok().await?, LnReceiveState::Funded);
            assert_eq!(sub1.ok().await?, LnReceiveState::AwaitingFunds);
            assert_eq!(sub1.ok().await?, LnReceiveState::Claimed);
        }
        _ => panic!("Expected internal payment!"),
    }

    // Pay the invoice again and verify that it does not deduct the balance, but it
    // does return the preimage
    let prev_balance = client2.get_balance().await;
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
        }
        _ => panic!("Expected internal payment!"),
    }

    let same_balance = client2.get_balance().await;
    assert_eq!(prev_balance, same_balance);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn gateway_protects_preimage_for_payment() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let gw = gateway(&fixtures, &fed).await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();

    // Print money for client1
    let (op, outpoint) = client1_dummy_module.print_money(sats(10000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    // Print money for client2
    let (op, outpoint) = client2_dummy_module.print_money(sats(10000)).await?;
    client2.await_primary_module_output(op, outpoint).await?;

    let cln = fixtures.cln().await;
    let invoice = cln.invoice(Amount::from_sats(100), None).await?;

    // Pay invoice with client1
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client1, invoice.clone()).await?;
    match payment_type {
        PayType::Lightning(operation_id) => {
            let mut sub = client1
                .get_first_module::<LightningClientModule>()
                .subscribe_ln_pay(operation_id)
                .await?
                .into_stream();

            assert_eq!(sub.ok().await?, LnPayState::Created);
            assert_eq!(sub.ok().await?, LnPayState::Funded);
            assert_matches!(sub.ok().await?, LnPayState::Success { .. });
        }
        _ => panic!("Expected lightning payment!"),
    }

    // Verify that client2 cannot pay the same invoice and the preimage is not
    // returned
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice.clone()).await?;
    match payment_type {
        PayType::Lightning(operation_id) => {
            let mut sub = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_ln_pay(operation_id)
                .await?
                .into_stream();

            assert_eq!(sub.ok().await?, LnPayState::Created);
            assert_eq!(sub.ok().await?, LnPayState::Funded);
            assert_matches!(sub.ok().await?, LnPayState::WaitingForRefund { .. });
            assert_matches!(sub.ok().await?, LnPayState::Refunded { .. });
        }
        _ => panic!("Expected lightning payment!"),
    }

    drop(gw);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn cannot_pay_same_external_invoice_twice() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let client = fed.new_client().await;
    let gw = gateway(&fixtures, &fed).await;
    let dummy_module = client.get_first_module::<DummyClientModule>();

    // Print money for client
    let (op, outpoint) = dummy_module.print_money(sats(1000)).await?;
    client.await_primary_module_output(op, outpoint).await?;

    let cln = fixtures.cln().await;
    let invoice = cln.invoice(Amount::from_sats(100), None).await?;

    // Pay the invoice for the first time
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client, invoice.clone()).await?;
    match payment_type {
        PayType::Lightning(operation_id) => {
            let mut sub = client
                .get_first_module::<LightningClientModule>()
                .subscribe_ln_pay(operation_id)
                .await?
                .into_stream();

            assert_eq!(sub.ok().await?, LnPayState::Created);
            assert_eq!(sub.ok().await?, LnPayState::Funded);
            assert_matches!(sub.ok().await?, LnPayState::Success { .. });
        }
        _ => panic!("Expected lightning payment!"),
    }

    let prev_balance = client.get_balance().await;

    // Pay the invoice again and verify that it does not deduct the balance, but it
    // does return the preimage
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client, invoice).await?;
    match payment_type {
        PayType::Lightning(operation_id) => {
            let mut sub = client
                .get_first_module::<LightningClientModule>()
                .subscribe_ln_pay(operation_id)
                .await?
                .into_stream();

            assert_eq!(sub.ok().await?, LnPayState::Created);
            assert_eq!(sub.ok().await?, LnPayState::Funded);
            assert_matches!(sub.ok().await?, LnPayState::Success { .. });
        }
        _ => panic!("Expected lightning payment!"),
    }

    let same_balance = client.get_balance().await;
    assert_eq!(prev_balance, same_balance);

    drop(gw);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn makes_internal_payments_within_federation() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();

    // Print money for client2
    let (op, outpoint) = client2_dummy_module.print_money(sats(1000)).await?;
    client2.await_primary_module_output(op, outpoint).await?;

    // TEST internal payment when there are no gateways registered
    let (op, invoice, _) = client1
        .get_first_module::<LightningClientModule>()
        .create_bolt11_invoice(sats(250), "with-markers".to_string(), None, ())
        .await?;
    let mut sub1 = client1
        .get_first_module::<LightningClientModule>()
        .subscribe_ln_receive(op)
        .await?
        .into_stream();
    assert_eq!(sub1.ok().await?, LnReceiveState::Created);
    assert_matches!(sub1.ok().await?, LnReceiveState::WaitingForPayment { .. });

    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
            assert_eq!(sub1.ok().await?, LnReceiveState::Funded);
            assert_eq!(sub1.ok().await?, LnReceiveState::AwaitingFunds);
            assert_eq!(sub1.ok().await?, LnReceiveState::Claimed);
        }
        _ => panic!("Expected internal payment!"),
    }

    // TEST internal payment when there is a registered gateway
    gateway(&fixtures, &fed).await;

    let (op, invoice, _) = client1
        .get_first_module::<LightningClientModule>()
        .create_bolt11_invoice(sats(250), "with-gateway-hint".to_string(), None, ())
        .await?;
    let mut sub1 = client1
        .get_first_module::<LightningClientModule>()
        .subscribe_ln_receive(op)
        .await?
        .into_stream();
    assert_eq!(sub1.ok().await?, LnReceiveState::Created);
    assert_matches!(sub1.ok().await?, LnReceiveState::WaitingForPayment { .. });

    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
            assert_eq!(sub1.ok().await?, LnReceiveState::Funded);
            assert_eq!(sub1.ok().await?, LnReceiveState::AwaitingFunds);
            assert_eq!(sub1.ok().await?, LnReceiveState::Claimed);
        }
        _ => panic!("Expected internal payment!"),
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn external_payments() -> anyhow::Result<()> {
    use futures::stream::StreamExt;

    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client3 = fed.new_client().await;
    gateway(&fixtures, &fed).await;

    // Print money for client2
    let client2_dummy_module = client2.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client2_dummy_module.print_money(sats(1000)).await?;
    client2.await_primary_module_output(op, outpoint).await?;

    let payment_key = KeyPair::new(secp256k1::SECP256K1, &mut thread_rng());

    // TEST internal payment using external invoice generation when there are no
    // gateways registered
    //
    // Client 1: generates invoice
    // Client 2: pays invoice
    // Client 3: claims invoice

    // Generate invoice
    info!("Generating invoice");
    let (_op, invoice, _) = client1
        .get_first_module::<LightningClientModule>()
        .create_bolt11_invoice_ext(
            payment_key.public_key(),
            sats(250),
            "with-markers".to_string(),
            None,
            (),
        )
        .await?;

    sleep(Duration::from_secs(1)).await;

    // Pay invoice
    info!("Paying invoice");
    let OutgoingLightningPayment {
        payment_type,
        contract_id: _,
        fee: _,
    } = pay_invoice(&client2, invoice).await?;
    match payment_type {
        PayType::Internal(op_id) => {
            let mut sub2 = client2
                .get_first_module::<LightningClientModule>()
                .subscribe_internal_pay(op_id)
                .await?
                .into_stream();
            assert_eq!(sub2.ok().await?, InternalPayState::Funding);
            assert_matches!(sub2.ok().await?, InternalPayState::Preimage { .. });
        }
        _ => panic!("Expected internal payment!"),
    }

    sleep(Duration::from_secs(1)).await;
    // Claim invoice
    info!("Claiming invoice");
    let client3_ln_module = client3.get_first_module::<LightningClientModule>();
    let (_operation, _amt) = client3_ln_module
        .claim_external_bolt11_invoice(payment_key, ())
        .await
        .unwrap();
    // FIXME
    // assert_eq!(amt, sats(250));

    info!("Waiting for client 3 to receive 250sat");

    let mut balance_sub = client3.subscribe_balance_changes().await;
    while let Some(balance) = balance_sub.next().await {
        if balance == sats(250) {
            break;
        }
        info!("Balance now {}", balance);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_wrong_network_invoice() -> anyhow::Result<()> {
    let fixtures = fixtures();
    let fed = fixtures.new_fed().await;
    let client1 = fed.new_client().await;
    gateway(&fixtures, &fed).await;

    // Signet invoice should fail on regtest
    let signet_invoice = Bolt11Invoice::from_str(
        "lntbs1u1pj8308gsp5xhxz908q5usddjjm6mfq6nwc2nu62twwm6za69d32kyx8h49a4hqpp5j5egfqw9kf5e96nk\
        6htr76a8kggl0xyz3pzgemv887pya4flguzsdp5235xzmntwvsxvmmjypex2en4dejxjmn8yp6xsefqvesh2cm9wsss\
        cqp2rzjq0ag45qspt2vd47jvj3t5nya5vsn0hlhf5wel8h779npsrspm6eeuqtjuuqqqqgqqyqqqqqqqqqqqqqqqc9q\
        yysgqddrv0jqhyf3q6z75rt7nrwx0crxme87s8rx2rt8xr9slzu0p3xg3f3f0zmqavtmsnqaj5v0y5mdzszah7thrmg\
        2we42dvjggjkf44egqheymyw",
    )
    .unwrap();

    let error = pay_invoice(&client1, signet_invoice).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "Invalid invoice currency: expected=Regtest, got=Signet"
    );

    Ok(())
}
