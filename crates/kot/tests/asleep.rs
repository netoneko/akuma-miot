//! `kot run --asleep`: who a sleeping cat answers, and who it doesn't.
//! The loop around this is a poll and a `say`; the rule is what can go wrong
//! (a reply to every broadcast, or two sleeping cats answering each other).

use kot::agent::{asleep_owes_reply, asleep_reply};
use serde_json::json;

const ME: &str = "aa";
const ROOT: &str = "bb";

fn said(from: &str, to: Option<&str>, body: &str, no_ack: bool) -> serde_json::Value {
    json!({"t":"said","from":from,"to":to,"body":body,"root":false,"no_ack":no_ack,"off_record":false})
}

#[test]
fn a_dm_gets_the_reply_back_to_its_sender() {
    let e = said(ROOT, Some(ME), "yuki, how's it going?", false);
    assert_eq!(asleep_owes_reply("yuki", ME, &e, Some(ME)), Some(ROOT.to_string()));
    assert_eq!(asleep_reply("yuki"), "*yuki is currently asleep*");
}

#[test]
fn a_broadcast_tag_gets_it_and_an_untagged_broadcast_does_not() {
    let tagged = said(ROOT, None, "hey @Yuki and @shiro, status?", false);
    assert_eq!(asleep_owes_reply("yuki", ME, &tagged, Some("*")), Some(ROOT.to_string()));
    let plain = said(ROOT, None, "morning, litter", false);
    assert_eq!(asleep_owes_reply("yuki", ME, &plain, Some("*")), None);
    let other = said(ROOT, None, "@yukimura is someone else", false);
    assert_eq!(asleep_owes_reply("yuki", ME, &other, Some("*")), None, "a longer name is not a tag");
}

#[test]
fn a_message_tag_counts_too() {
    let e = json!({"t":"message","from":ROOT,"to":null,"body":"see thread","tags":["@yuki"],"no_ack":false});
    assert_eq!(asleep_owes_reply("yuki", ME, &e, Some("*")), Some(ROOT.to_string()));
}

#[test]
fn no_reply_to_itself_to_no_ack_or_to_anything_else() {
    assert_eq!(asleep_owes_reply("yuki", ME, &said(ME, Some(ME), "note to self", false), Some(ME)), None);
    // Another sleeping cat's reply is `no_ack`: answering it would never end.
    let their_reply = said(ROOT, Some(ME), "*shiro is currently asleep*", true);
    assert_eq!(asleep_owes_reply("yuki", ME, &their_reply, None), None);
    let task = json!({"t":"assigned","to":ME,"task":"t1","what":"x","expect":"y"});
    assert_eq!(asleep_owes_reply("yuki", ME, &task, Some(ME)), None, "work is ignored, not answered");
    let dm_elsewhere = said(ROOT, Some("cc"), "for someone else", false);
    assert_eq!(asleep_owes_reply("yuki", ME, &dm_elsewhere, Some("cc")), None);
}
