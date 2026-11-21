use super::*;

#[test]
fn an_open_answer_means_what_its_consumer_states() {
    assert_eq!(Answer::Known(3).open_means(0), 3);
    assert_eq!(Answer::<u32>::Open.open_means(0), 0);
    assert!(Answer::<u32>::Open.is_open());
    assert!(!Answer::Known(1).is_open());
}

#[test]
fn combined_answers_are_open_when_one_part_is() {
    let known = Answer::Known(2);
    let open = Answer::<u32>::Open;
    assert_eq!(known.as_ref(), Answer::Known(&2));
    assert_eq!(open.as_ref(), Answer::Open);
    assert_eq!(known.map(|x| x * 2), Answer::Known(4));
    assert_eq!(open.map(|x| x * 2), Answer::Open);
    assert_eq!(known.and_then(|x| Answer::Known(x + 1)), Answer::Known(3));
    assert_eq!(known.and_then(|_| Answer::<u32>::Open), Answer::Open);
    assert_eq!(open.and_then(|x| Answer::Known(x + 1)), Answer::Open);
    assert_eq!(known.zip(Answer::Known("a")), Answer::Known((2, "a")));
    assert_eq!(known.zip(Answer::<&str>::Open), Answer::Open);
    assert_eq!(open.zip(Answer::Known("a")), Answer::Open);
    assert_eq!(
        known.join_with(Answer::Known(5), u32::max),
        Answer::Known(5)
    );
    assert_eq!(known.join_with(open, u32::max), Answer::Open);
}

#[test]
fn collected_answers_are_established_only_when_each_is() {
    let all: Answer<Vec<u32>> = [Answer::Known(1), Answer::Known(2)].into_iter().collect();
    assert_eq!(all, Answer::Known(vec![1, 2]));
    let one_open: Answer<Vec<u32>> = [Answer::Known(1), Answer::Open, Answer::Known(3)]
        .into_iter()
        .collect();
    assert_eq!(one_open, Answer::Open);
    let none: Answer<Vec<u32>> = std::iter::empty().collect();
    assert_eq!(none, Answer::Known(Vec::new()));
}

#[test]
fn answers_serialize_by_their_state() {
    let json = |a: &Answer<u32>| serde_json::to_string(a).unwrap();
    assert_eq!(json(&Answer::Known(4)), r#"{"known":4}"#);
    assert_eq!(json(&Answer::Open), r#""open""#);
    let back: Answer<u32> = serde_json::from_str(r#"{"known":4}"#).unwrap();
    assert_eq!(back, Answer::Known(4));
}
