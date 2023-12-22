use std::io::Cursor;
use std::time::Duration;

use fedimint_client::backup::{ClientBackup, Metadata};
use fedimint_core::task::sleep;
use fedimint_core::util::NextOrPending;
use fedimint_core::{sats, Amount};
use fedimint_dummy_client::{DummyClientInit, DummyClientModule};
use fedimint_dummy_common::config::DummyGenParams;
use fedimint_dummy_server::DummyInit;
use fedimint_mint_client::{
    MintClientInit, MintClientModule, OOBNotes, ReissueExternalNotesState, SpendOOBState,
};
use fedimint_mint_common::config::MintGenParams;
use fedimint_mint_server::MintInit;
use fedimint_testing::fixtures::{Fixtures, TIMEOUT};
use futures::StreamExt;
use tracing::info;

fn fixtures() -> Fixtures {
    let fixtures = Fixtures::new_primary(MintClientInit, MintInit, MintGenParams::default());
    fixtures.with_module(DummyClientInit, DummyInit, DummyGenParams::default())
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_ecash_out_of_band() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    // Spend from client1 to client2
    let client1_mint = client1.get_first_module::<MintClientModule>();
    let client2_mint = client2.get_first_module::<MintClientModule>();
    let (op, notes) = client1_mint.spend_notes(sats(750), TIMEOUT, ()).await?;
    let sub1 = &mut client1_mint.subscribe_spend_notes(op).await?.into_stream();
    assert_eq!(sub1.ok().await?, SpendOOBState::Created);

    let op = client2_mint.reissue_external_notes(notes, ()).await?;
    let sub2 = client2_mint.subscribe_reissue_external_notes(op).await?;
    let mut sub2 = sub2.into_stream();
    assert_eq!(sub2.ok().await?, ReissueExternalNotesState::Created);
    assert_eq!(sub2.ok().await?, ReissueExternalNotesState::Issuing);
    assert_eq!(sub2.ok().await?, ReissueExternalNotesState::Done);
    assert_eq!(sub1.ok().await?, SpendOOBState::Success);

    assert_eq!(client1.get_balance().await, sats(250));
    assert_eq!(client2.get_balance().await, sats(750));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_ecash_oob_highly_parallel() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let (client1, client2) = fed.two_clients().await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    const NUM_PAR_REISSUE: usize = 10;

    // Spend from client1 to client2 25 times in parallel
    let mut spend_tasks = vec![];
    for num_spend in 0..NUM_PAR_REISSUE {
        let task_client1 = client1.clone();
        spend_tasks.push(tokio::spawn(async move {
            info!("Starting spend {num_spend}");
            let client1_mint = task_client1.get_first_module::<MintClientModule>();
            let (op, notes) = client1_mint
                .spend_notes(sats(30), TIMEOUT, ())
                .await
                .unwrap();
            let sub1 = &mut client1_mint
                .subscribe_spend_notes(op)
                .await
                .unwrap()
                .into_stream();
            assert_eq!(sub1.ok().await.unwrap(), SpendOOBState::Created);
            notes
        }));
    }

    let note_bags = futures::stream::iter(spend_tasks)
        .then(|handle| async move { handle.await.unwrap() })
        .collect::<Vec<_>>()
        .await;
    let total_amount = note_bags
        .iter()
        .map(|notes| notes.total_amount())
        .sum::<Amount>();

    dbg!(note_bags.iter().map(|n| n.to_string()).collect::<Vec<_>>());

    let mut reissue_tasks = vec![];
    for (num_reissue, notes) in note_bags.into_iter().enumerate() {
        let task_client2 = client2.clone();
        reissue_tasks.push(tokio::spawn(async move {
            info!("Starting reissue {num_reissue}");
            let client2_mint = task_client2.get_first_module::<MintClientModule>();
            let op = client2_mint
                .reissue_external_notes(notes, ())
                .await
                .unwrap();
            let sub2 = client2_mint
                .subscribe_reissue_external_notes(op)
                .await
                .unwrap();
            let mut sub2 = sub2.into_stream();
            assert_eq!(sub2.ok().await.unwrap(), ReissueExternalNotesState::Created);
            assert_eq!(sub2.ok().await.unwrap(), ReissueExternalNotesState::Issuing);
            assert_eq!(sub2.ok().await.unwrap(), ReissueExternalNotesState::Done);
        }));
    }

    for task in reissue_tasks {
        task.await.unwrap();
    }

    assert_eq!(client1.get_balance().await, sats(1000) - total_amount);
    assert_eq!(client2.get_balance().await, total_amount);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn backup_encode_decode_roundtrip() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let (client1, _client2) = fed.two_clients().await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    let backup = client1.create_backup(Metadata::empty()).await?;

    let backup_bin =
        fedimint_core::encoding::Encodable::consensus_encode_to_vec(&backup).expect("encode");

    let backup_decoded: ClientBackup = fedimint_core::encoding::Decodable::consensus_decode(
        &mut Cursor::new(&backup_bin),
        client1.decoders(),
    )
    .expect("decode");

    assert_eq!(backup, backup_decoded);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_ecash_out_of_band_cancel() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let client = fed.new_client().await;
    let dummy_module = client.get_first_module::<DummyClientModule>();
    let (op, outpoint) = dummy_module.print_money(sats(1000)).await?;
    client.await_primary_module_output(op, outpoint).await?;

    // Spend from client1 to client2
    let mint_module = client.get_first_module::<MintClientModule>();
    let (op, _) = mint_module.spend_notes(sats(750), TIMEOUT, ()).await?;
    let sub1 = &mut mint_module.subscribe_spend_notes(op).await?.into_stream();
    assert_eq!(sub1.ok().await?, SpendOOBState::Created);

    mint_module.try_cancel_spend_notes(op).await;
    assert_eq!(sub1.ok().await?, SpendOOBState::UserCanceledProcessing);
    assert_eq!(sub1.ok().await?, SpendOOBState::UserCanceledSuccess);

    info!("Refund tx accepted, waiting for refunded e-cash");

    // FIXME: UserCanceledSuccess should mean the money is in our wallet
    for _ in 0..200 {
        sleep(Duration::from_millis(100)).await;
        if client.get_balance().await == sats(1000) {
            return Ok(());
        }
    }

    panic!("Did not receive refund in time");
}

#[tokio::test(flavor = "multi_thread")]
async fn error_zero_value_oob_spend() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let (client1, _client2) = fed.two_clients().await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    // Spend from client1 to client2
    let err_msg = client1
        .get_first_module::<MintClientModule>()
        .spend_notes(Amount::ZERO, TIMEOUT, ())
        .await
        .expect_err("Zero-amount spends should be forbidden")
        .to_string();
    assert!(err_msg.contains("zero-amount"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn error_zero_value_oob_receive() -> anyhow::Result<()> {
    // Print notes for client1
    let fed = fixtures().new_fed().await;
    let (client1, _client2) = fed.two_clients().await;
    let client1_dummy_module = client1.get_first_module::<DummyClientModule>();
    let (op, outpoint) = client1_dummy_module.print_money(sats(1000)).await?;
    client1.await_primary_module_output(op, outpoint).await?;

    // Spend from client1 to client2
    let err_msg = client1
        .get_first_module::<MintClientModule>()
        .reissue_external_notes(
            OOBNotes::new(client1.federation_id().to_prefix(), Default::default()),
            (),
        )
        .await
        .expect_err("Zero-amount receives should be forbidden")
        .to_string();
    assert!(err_msg.contains("zero-amount"));

    Ok(())
}
