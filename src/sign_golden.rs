// Golden vector computed with viem 2.x: hashTypedData + signTypedData for KEY1 on the fixed
// OpenPosition in `fixed_open()`, domain {name:"String",version:"1",chainId:999,exchange}.
#[test]
fn golden_matches_viem() {
    let signer = PrivateKeySigner::from_str(KEY1).unwrap();
    let domain = exchange_domain(999, Address::from_str(EXCHANGE).unwrap());
    let s = sign(&signer, &domain, &fixed_open()).unwrap();
    assert_eq!(format!("{:#x}", s.digest), "0xdffc77551a982054592cb1316bd089464f1e7c83d274dbc86b94887c199fe566");
    assert_eq!(s.body["r"], "0x272f983e7225aacd40cb7398fc43b568ed70f457719a942eb366f8d3193cd4e9");
    assert_eq!(s.body["s"], "0x67649d4a120f6d069c6632b6f13fa79a3eeb5d1b563a65a7cd6065e162d99586");
    assert_eq!(s.body["v"], "28");
    assert_eq!(format!("{:#x}", s.intent_id), "0x45fe8fcd001b5c86516cdb2ccaada3997abcf60f17d14205e4efe5bc97711013");
}
