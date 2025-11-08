use navpath_core::eligibility::*;

#[test]
fn numeric_vs_string_satisfied() {
    // Build three tags: coins>=100 (num), varbit_38==1 (num), quest_state==done (str)
    let tags: Vec<EncodedTag> = vec![
        [1, fnv1a32("coins"), encode_opbits(Op::Ge, true), (100i32 as u32)],
        [2, fnv1a32("varbit_38"), encode_opbits(Op::Eq, true), (1i32 as u32)],
        [3, fnv1a32("quest_state"), encode_opbits(Op::Eq, false), fnv1a32("done")],
    ];
    let mut words = Vec::<u32>::new();
    for t in &tags { words.extend_from_slice(t); }

    let mask = build_mask_from_u32(
        &words,
        [
            ("coins", ClientValue::Num(150)),
            ("varbit_38", ClientValue::Num(1)),
            ("quest_state", ClientValue::Str("done")),
        ],
    );
    assert_eq!(mask.len(), 3);
    assert!(mask.is_satisfied(0));
    assert!(mask.is_satisfied(1));
    assert!(mask.is_satisfied(2));
}

#[test]
fn unsatisfied_mixed_types_and_ops() {
    // membership!=ironman (str), level>50 (num), skill<=20 (num)
    let tags: Vec<EncodedTag> = vec![
        [10, fnv1a32("membership"), encode_opbits(Op::Ne, false), fnv1a32("ironman")],
        [11, fnv1a32("level"), encode_opbits(Op::Gt, true), (50i32 as u32)],
        [12, fnv1a32("skill"), encode_opbits(Op::Le, true), (20i32 as u32)],
    ];
    let mut words = Vec::<u32>::new();
    for t in &tags { words.extend_from_slice(t); }

    let mask = build_mask_from_u32(
        &words,
        [
            ("membership", ClientValue::Str("ironman")), // Ne should be false
            ("level", ClientValue::Num(49)),             // not > 50
            ("skill", ClientValue::Str("20")),          // wrong type
        ],
    );
    assert_eq!(mask.satisfied, vec![false, false, false]);
}
