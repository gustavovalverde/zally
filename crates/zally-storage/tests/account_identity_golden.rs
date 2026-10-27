//! Account identity stays byte-stable across librustzcash cohort moves.
//!
//! An `AccountId` is the UUID v5 of the network-encoded UFVK, and operators hand out Unified
//! Address strings, so a change in either encoding silently changes a wallet's identity or
//! the text of an address a payer already holds. The goldens below were captured from a fixed
//! seed on the `zcash_keys 0.16.0` / `zcash_client_sqlite 0.22.0-rc.4` stack (Zally
//! `4eaa0bb`). A cohort move that changes any of them must change this file in the same
//! commit and document why.
//!
//! Shielded-only addresses from `derive_next_address` use a clock-derived diversifier index,
//! so their text is not a golden. The test pins their shape instead: each must be the UFVK's
//! shielded address at the index the wallet recorded for it.

use tempfile::TempDir;
use zally_core::{AccountId, BlockHeight, Network};
use zally_keys::{KeyDerivationError, SeedMaterial, SeedMaterialError, derive_ufvk};
use zally_storage::{Sqlite, SqliteOptions, StorageError, WalletStorage};
use zcash_keys::keys::{AddressGenerationError, UnifiedAddressRequest, UnifiedFullViewingKey};
use zip32::DiversifierIndex;

#[path = "fixtures/scan_artifact.rs"]
mod scan_artifact;

const GOLDEN_SEED: [u8; 32] = [0x5a; 32];

/// Diversifier index probed for a shielded-only encoding; it has no valid Sapling diversifier
/// on every network below, so the address carries an Orchard receiver alone.
const SHIELDED_PROBE_INDEX: u64 = 4_610_435_845;

struct NetworkGolden {
    label: &'static str,
    chain_tip_height: u32,
    ufvk: &'static str,
    account_id: &'static str,
    default_address: &'static str,
    default_shielded_address: &'static str,
    shielded_address_at_probe_index: &'static str,
    transparent_address_at_index_7: &'static str,
    first_transparent_derivation: &'static str,
    first_transparent_derivation_index: u128,
}

const REGTEST: NetworkGolden = NetworkGolden {
    label: "regtest",
    chain_tip_height: 10,
    ufvk: "uviewregtest16j9etpkfhcq4huc39f3a3y0zzxjdh59cq8fpgahp8sl9aylrh5hucyfh286qsltwuqv05z6n07297vfyakjexpf082nkk6ex065xy4dmsyd3992pnwmrawn8dswcc8myu68uvvsfnmw0tadmqxu8x80w6z050q48d6jzax804fc9nv8fngsg4nd4ft7dkwp6yt9cfhf64hfnf7mshz7hcasf8nkt4ltlnt8luw79q33l79vr26wqkw32ga4nndtjv6w35tycelvswr4agcwwqmfrk6uun580gjvd53myymfwsqargf658cw7j9lnklw0fkf629kd9qlq4xdlgtumm5c0qfplt96ejtdsc6g38g2xkg0sym259sqnk6vse9l43wrs3far8gqvg207wkqhkenramkjg7uc9ftdlfpz6mlnrfa4fc63z2sex5mx7cnjr8n9fn979v4ucg28e5ly4maerdhjmtn9hnw602w4ejw43cy2f5yyerhg",
    account_id: "3da7c108-9998-5afe-bdfb-bb0e96471930",
    default_address: "uregtest145pra82mppvy7nlq02ttjc9rs25ykxqr9vn34smkcg82pkj03fkj4xmh2k5e2l25uzcea0xd374gt7r4guzl9z6t2fq8qdfaznaafq9p8nkszyfeyyg00da57w0t7md7npan0cqu7lj77p8svgcyx98f0m8uj2hq0nfwf2nmqavgdxm399njtcyja409kahthgzwz2m96w9ag2gw4yd",
    default_shielded_address: "uregtest1rkm45vvey70326ke0emcqhp3zzsymsjryddedy5qzmz3ralnaghsyr63j73jqlptatktlfjsc76qvlnfudeqxkd4py50rauzlhsg8tc9ugx35hdg5f277u7uspwadykl04wxrhghvtvaav7ss5zadj2y5zsd8w5e7tl2d8p4vq5xxcwl",
    shielded_address_at_probe_index: "uregtest1hsscsevg3rxnvcptlcmep55jztrx35ygxw95k2mzs4fvpayu2v954rkfmp782ngygvxchg3qshj5800avvg4yw4cjhmznu32quf2hg8l",
    transparent_address_at_index_7: "uregtest1k7dm5rmrhsmq0gw8q05dyrt8pkgvhm549wm7w9a0x5rqc94nucjnky6crjt2ug9z9hqfdlyquuelfvaucd83uvu50kzqsg5nxawrldtth8gvjvhtwfsrl8wk3jkhe746mps6nnfanrkh8anq9ywzxhj0cereva9a3jlpetjgh80pae6vy4kr76y8ugf57ns3tu5wa6n8ueha7tr903q",
    first_transparent_derivation: "uregtest1cxlg5f3fnzunlu66y7mq7rupyexvle0nv7rdtk7sa3j7qjtwx0p7lpzrpup58q6g3mrhrv54hvl8zcpq2alvrheyc0399vnh2ee5dhjuaj534pts62jpwfuntatllas3k86advkfv2h4976hndhz57d2kdtutu46r8fg467e0va94w2sxtfhfnrxvqv4p60hev0hwvqt3yza55qy7mr",
    first_transparent_derivation_index: 2,
};

const MAINNET: NetworkGolden = NetworkGolden {
    label: "mainnet",
    chain_tip_height: 419_210,
    ufvk: "uview1hc2q2lst6jp5ql960y3qgnd6cvqnvvezx0d5qvwz8z56hptc9q8aaqt277phgv6yj284p56s8a8slw2kl9erv9vq6tycswku0pllm2alc2svjdwuy548lpk5e9efdyv8cv4dawjcdnxnap0092ns7hw5x70j8tj07n6fl52g2jjacypmlhwd36sl2yueu46vhfc7l4yh795g9cfhtart8jtqs4xr96xv5swjw7un2r8arf5mn832cvj89whtc26dk7ujn5qplx87m9wd4drkvgargywqrm2394q3p4q60zu4cyl6ht5p0kxvayhjze97sjzj2utejpsd9pvpxy8ew7gyrrn2x38mm3p2pgstgcsgxu98t674mghkd459g627ftpdg5munz5yy0fu9yvnrtltfd4u3gyyy6y8wasnhj7qdzp5nc9qd34mhavgvtl9az3fqdztgcnad4xjc04hy9732vdat2e3nxpnfk8uhmt8njjxmqll7w0v",
    account_id: "276f2fbd-a236-5c23-a278-64f7bd26a0ae",
    default_address: "u1hlqaetnhrrudhpsd4sulp3lx42jp59wla4wf39lt3p8v0784qksjnz7kzq59ww0s8rtppjmp39n60s2h54nnx0dzwll96gtwznzhukw64sxh9lfy0qwk97vmvvycf95mvn4pt785drqrpku8wjnn4jf072agqdynmqcsa7u027z3xa5jgnjmhsu32aqn7dzd477xcrjalst5sw7jz02",
    default_shielded_address: "u1a6qwptvjetln0lyx9zsmscyx5xnev8pweyyuqq4du4474zehl8amk0y52af8s82kqctdg5tn4uzej9fneg65eyy98xmkxy06aphfxus63mlyygmj2tr7xdqlhvv2akgcnkkdhc76xus9n2hujzdklvj99cpxs9e8kmwc7gh03gq744sg",
    shielded_address_at_probe_index: "u1nqqgls9k08a04xrak3kmc4zc6tcej7wppje6778hmhnpdx826lj2vlpnzmxydxjygjxgssgfxzpufasprxk4lptdg2rkcmzrzgmslcku",
    transparent_address_at_index_7: "u18hxqtfczcy0edctljperm5rg87tw869u25e3eusmur8amj8umu9hy92y8fly6hmjpq89rn640mgecn02tu62a8vfl74ajclcusmtujxkc620hx62x3rrxhj7v2tkzha5u8x2zs7zztd7d7lttpjgxuzf80u4kznfln82t0muugpdpkzj9ht0d02r367d4e9l5g5ntakrwk2yxverqe4",
    first_transparent_derivation: "u1rc2s99uku0y9774h2qhcufteuu2dkkk4meha6jlzwexq3lxkpuvfyld703xkefmuse3kfkrcrnfzzrgjvqn37mtq3c9ptah8d04zs5pktc3sq8esm2w0tqprek0zqpasu7ctrra6lyjag893jlgpt57wulxsz2ymwmth64skhl9nljha8uwe275yj4gc8pggzmu68m325fn8qsskfgm",
    first_transparent_derivation_index: 1,
};

const TESTNET: NetworkGolden = NetworkGolden {
    label: "testnet",
    chain_tip_height: 280_010,
    ufvk: "uviewtest1zlnvz0ldzy6798es72wvqqcpxykjq5fuuwuxckfpdhjv6xxxdznsyvcezpxwkyzj44e3wsvvxj50huu3pw7qlhl4302p37m4llnl28hayk2j9zdqnnerms2lp3vfxdndzc7j30ewm0yec6a848hjzlrv6d7r54838qnspt67wcmp350gksqvc8nw8f52gg4eel85ue9z655038awtwappp9k7x2nu9fgr53k0knxr6yc95ztrqa3gv4qrf8nskh9dj7xfy9ewyn5elmw4zmktw55778h5ks7cpzj9fa7gwxtzpq2nnat9w0y5cmml4q5e2tnk0994hrqvjseu8y9gqf56hnylzrfwmegxxzy9wuxvpxnsy5r50s95xrlvu7hd04y3df0y5hrtfuyl0jkx0sgeuhefz3lm0y2ml7wq5zekxyy20da0q8qqk2jnpv48h2u3n8qlfzesemgf5kjz35muq27pt2gy4arn3zup06e8xx335m5675y",
    account_id: "96d98677-f009-584d-84d9-8d91e219314b",
    default_address: "utest1avqmg5ewarmdlqsvzd46pl7hpedacz7q9f75aatgthvfe34ljs08tx4xzn6evk5j0mwp4cyy6a7ced79erfv9w6stkyx6zzhzhae6tz8ar29ej8r8980cca5zkvkhx7dshsd3zfdmxsgqa9j0wze0exf99er4uzdqax32xp69t02fedd80u53rucyk87vd8ujdpnfxl6r8hk7vxs5vp",
    default_shielded_address: "utest1df9zs453uplprpewzuzkzkm2qv6n65q7cy9p43xsgm7k2zqc5zpafqtf7pr3nyj8zec9eqdnf0jpa0mjvfwgr5gxhmsg2hc98x2wgr9wndj7aqkd4hel6laf82wjzrnqa2hjpn98ptee4vgnvrl4srmykh5mmq8ztwjt0dy4ru8rfq9j",
    shielded_address_at_probe_index: "utest1du2ca2pru8zmygyhcy3j8xy93j8yh6uvu53a3gwp5h50ynx2eqtfnvaqzevgexgrctn3xxl2cus4t0g3zlut0d35c54z4mywjvnzzhlx",
    transparent_address_at_index_7: "utest1wxraz94mlvlq0rsc7vsucpatyk6yqjxr0qrldptkte3gckv3r7t06lq3vt62l9jfwtfghz8j6cmryrsfqn07sul48kkv68hnyhydft9vjt96ke23sck3qxxvgfq72pg2ussh4s4urzpj40pvp9ujtxgeul7sanmaglcu5qzg9qel7xv08yvyrvryxnta8v5r6vhz9kkq30pau97wmue",
    first_transparent_derivation: "utest13tne7pvnemwyfq0frfy8m3xd4cgyfanseejz25huvqnxta7y5qgxr63kx8kdc8ucj5ftgzsk7j0gghhy22d9kdysna9ukz8wkr2lcgztscpzkj2n2sjqdsjdzv4ll4s6t4gapr2elwn3p6sut6kyzvyr3gjn0pz0j9g79qnumjrchq9nlejlrx4sjd76nreght3sgakgrda2xcudjlh",
    first_transparent_derivation_index: 2,
};

fn goldens() -> [(Network, NetworkGolden); 3] {
    [
        (Network::regtest(), REGTEST),
        (Network::Mainnet, MAINNET),
        (Network::Testnet, TESTNET),
    ]
}

fn golden_seed() -> Result<SeedMaterial, TestError> {
    Ok(SeedMaterial::from_raw_bytes(GOLDEN_SEED.to_vec())?)
}

fn golden_ufvk(network: Network) -> Result<UnifiedFullViewingKey, TestError> {
    Ok(derive_ufvk(
        network,
        &golden_seed()?,
        zip32::AccountId::ZERO,
    )?)
}

fn encode_ufvk(network: Network, ufvk: &UnifiedFullViewingKey) -> String {
    ufvk.encode(&network.to_parameters())
}

fn encode_address(network: Network, address: &zcash_keys::address::UnifiedAddress) -> String {
    address.encode(&network.to_parameters())
}

async fn fresh_account(network: Network) -> Result<(Sqlite, AccountId, TempDir), TestError> {
    let temp = TempDir::new()?;
    let storage = Sqlite::new(SqliteOptions::for_network(
        network,
        temp.path().join("wallet.db"),
    ));
    storage.open_or_create().await?;
    let account_id = storage
        .create_account_for_seed(
            &golden_seed()?,
            scan_artifact::genesis_tree_state(network, 0, [0; 32]),
        )
        .await?;
    Ok((storage, account_id, temp))
}

#[test]
fn ufvk_encoding_matches_golden() -> Result<(), TestError> {
    for (network, golden) in goldens() {
        let encoded = encode_ufvk(network, &golden_ufvk(network)?);
        assert_eq!(
            encoded, golden.ufvk,
            "{} UFVK encoding drifted",
            golden.label
        );
    }
    Ok(())
}

#[test]
fn unified_address_encodings_match_golden() -> Result<(), TestError> {
    for (network, golden) in goldens() {
        let ufvk = golden_ufvk(network)?;
        let (default_address, default_index) =
            ufvk.default_address(UnifiedAddressRequest::AllAvailableKeys)?;
        assert_eq!(u128::from(default_index), 0);
        assert_eq!(
            encode_address(network, &default_address),
            golden.default_address,
            "{} default address drifted",
            golden.label
        );
        let (shielded_address, shielded_index) =
            ufvk.default_address(UnifiedAddressRequest::SHIELDED)?;
        assert_eq!(u128::from(shielded_index), 0);
        assert_eq!(
            encode_address(network, &shielded_address),
            golden.default_shielded_address,
            "{} default shielded address drifted",
            golden.label
        );
        let probe_address = ufvk.address(
            DiversifierIndex::from(SHIELDED_PROBE_INDEX),
            UnifiedAddressRequest::SHIELDED,
        )?;
        assert_eq!(
            encode_address(network, &probe_address),
            golden.shielded_address_at_probe_index,
            "{} Orchard-only address drifted",
            golden.label
        );
        let transparent_address = ufvk.address(
            DiversifierIndex::from(7_u32),
            UnifiedAddressRequest::AllAvailableKeys,
        )?;
        assert_eq!(
            encode_address(network, &transparent_address),
            golden.transparent_address_at_index_7,
            "{} transparent-bearing address drifted",
            golden.label
        );
    }
    Ok(())
}

#[tokio::test]
async fn account_id_matches_golden() -> Result<(), TestError> {
    for (network, golden) in goldens() {
        let (storage, account_id, _temp) = fresh_account(network).await?;
        assert_eq!(
            account_id.as_uuid().to_string(),
            golden.account_id,
            "{} AccountId drifted",
            golden.label
        );
        assert_eq!(
            storage.find_account_for_seed(&golden_seed()?).await?,
            Some(account_id),
            "{} lookup by seed must return the golden AccountId",
            golden.label
        );
    }
    Ok(())
}

#[tokio::test]
async fn derived_addresses_match_golden() -> Result<(), TestError> {
    for (network, golden) in goldens() {
        let (storage, account_id, _temp) = fresh_account(network).await?;
        storage
            .update_chain_tip(BlockHeight::from(golden.chain_tip_height))
            .await?;

        let transparent_address = storage
            .derive_next_address_with_transparent(account_id)
            .await?;
        assert_eq!(
            encode_address(network, &transparent_address),
            golden.first_transparent_derivation,
            "{} first transparent-bearing derivation drifted",
            golden.label
        );
        let shielded_address = storage.derive_next_address(account_id).await?;
        assert!(shielded_address.transparent().is_none());

        let ufvk = golden_ufvk(network)?;
        let exposed = storage.list_exposed_addresses(account_id).await?;
        let encoded: Vec<(String, u128, bool)> = exposed
            .iter()
            .map(|row| {
                (
                    encode_address(network, &row.unified_address),
                    u128::from(row.diversifier_index),
                    row.has_transparent_receiver,
                )
            })
            .collect();
        assert_eq!(
            encoded.get(..2),
            Some(
                &[
                    (golden.default_address.to_owned(), 0, true),
                    (
                        golden.first_transparent_derivation.to_owned(),
                        golden.first_transparent_derivation_index,
                        true,
                    ),
                ][..]
            ),
            "{} exposed default and transparent-bearing addresses drifted",
            golden.label
        );
        let shielded_row = exposed
            .iter()
            .find(|row| !row.has_transparent_receiver)
            .ok_or(TestError::MissingShieldedRow)?;
        let expected = ufvk.address(
            shielded_row.diversifier_index,
            UnifiedAddressRequest::SHIELDED,
        )?;
        assert_eq!(
            encode_address(network, &shielded_row.unified_address),
            encode_address(network, &expected),
            "{} shielded derivation must be the UFVK address at its recorded index",
            golden.label
        );
        assert_eq!(
            encode_address(network, &shielded_address),
            encode_address(network, &expected),
            "{} derive_next_address must return the recorded shielded address",
            golden.label
        );
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
enum TestError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("seed error: {0}")]
    Seed(#[from] SeedMaterialError),
    #[error("key derivation error: {0}")]
    KeyDerivation(#[from] KeyDerivationError),
    #[error("address generation error: {0:?}")]
    AddressGeneration(AddressGenerationError),
    #[error("no shielded-only exposed address was recorded")]
    MissingShieldedRow,
}

impl From<AddressGenerationError> for TestError {
    fn from(err: AddressGenerationError) -> Self {
        Self::AddressGeneration(err)
    }
}
