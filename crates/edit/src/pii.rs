//! The words of recognized text that look like personal data: six regular expressions,
//! matched by hand, each the way its first match would come out.

use nsxiv_ng_core::Rect;

use crate::annotation::inset;

/// A recognized word, its box in the image's pixels, origin top left, and the line it was read
/// in, the index of that line in the recognized text. A list of words holds each line's words
/// together and in reading order, as personal data across words is looked for among
/// neighbors of one line.
#[derive(Clone, Debug, PartialEq)]
pub struct Word {
    pub text: String,
    pub rect: Rect,
    pub line: usize,
}

/// The boxes of what is an email address, a phone number, a card number, a social security
/// number, an API key or a token, each grown by 3 pixels: a word, or words of one line, as a
/// number written in groups is. A word alone may hold a pattern's first match with up to two
/// characters of stray punctuation left over; anything looser would
/// censor ordinary words. Words of a line, joined by single spaces, may hold a match that
/// leaves two of their characters out in all, before it in the first word and after it in the
/// last, which censors the box around them all.
/// This errs on censoring more: a row of figures, as "1 2 3 4 5", or a date written with
/// spaces, as "2026 11 15", is a phone number's nine digits and spaces, and is censored too.
pub fn find_pii(words: &[Word]) -> Vec<Rect> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let last = span(words, i);
        if last > i || alone(&words[i].text) {
            let rect = (words[i + 1..=last].iter()).fold(words[i].rect, |r, w| union(r, w.rect));
            found.push(inset(rect, -3.0));
        }
        i = last + 1;
    }
    found
}

const PATTERNS: [Pattern; 6] = [email, phone, card, social_security, api_key, token];

/// Whether `word` alone holds a pattern's first match, two characters short of it at most.
fn alone(word: &str) -> bool {
    let chars: Vec<char> = word.chars().collect();
    // shortcut: the two left over are counted in characters rather than grapheme clusters, so
    // a word whose stray characters carry marks or joiners is missed; segment by UAX #29 if
    // text of such scripts shows it
    (PATTERNS.iter()).any(|pattern| {
        first_match(&chars, *pattern).is_some_and(|length| length + 2 >= chars.len())
    })
}

/// The last of the words of `first`'s line, from it on, that a pattern's longest match over
/// them covers, `first` itself where none covers more: a match within the words joined by
/// single spaces, which leaves at most two of their characters out, before it in the first word
/// and after it in the last, as a word alone may leave two.
fn span(words: &[Word], first: usize) -> usize {
    // an empty word starts no match, and its box joins none
    if words[first].text.is_empty() {
        return first;
    }
    let line = words[first].line;
    let mut chars = Vec::new();
    // where each word of the line from `first` on ends in the joined characters
    let mut ends = Vec::new();
    for word in words[first..].iter().take_while(|w| w.line == line) {
        if !chars.is_empty() {
            chars.push(' ');
        }
        chars.extend(word.text.chars());
        ends.push(chars.len());
    }
    // the last word first, then earlier ones, where a greedy match runs past a word's end
    let covers = |last: usize| {
        let (text, start_of_last) = (&chars[..ends[last]], ends[last - 1] + 1);
        (0..=2.min(ends[0] - 1)).any(|start| {
            PATTERNS.iter().any(|pattern| {
                pattern(text, start).is_some_and(|length| {
                    let end = start + length;
                    end > start_of_last && start + text.len() - end <= 2
                })
            })
        })
    };
    first
        + (1..ends.len())
            .rev()
            .find(|&last| covers(last))
            .unwrap_or(0)
}

/// The smallest rectangle around `a` and `b`.
fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect {
        x,
        y,
        width: a.max_x().max(b.max_x()) - x,
        height: a.max_y().max(b.max_y()) - y,
    }
}

/// A pattern's match at a position, as the regular expression's greedy match there: its
/// length in characters, or None.
type Pattern = fn(&[char], usize) -> Option<usize>;

/// The length of the leftmost match.
fn first_match(chars: &[char], pattern: Pattern) -> Option<usize> {
    (0..chars.len()).find_map(|start| pattern(chars, start))
}

/// How many characters from `at` are in `class`.
fn run(chars: &[char], at: usize, class: impl Fn(char) -> bool) -> usize {
    chars
        .get(at..)
        .map_or(0, |rest| rest.iter().take_while(|&&c| class(c)).count())
}

fn is(chars: &[char], at: usize, class: impl Fn(char) -> bool) -> bool {
    chars.get(at).is_some_and(|&c| class(c))
}

/// A regular expression's Unicode `\d`, any decimal digit of any script. This takes letter
/// and other numbers too (Ⅻ, ½), which errs on censoring more.
fn digit(c: char) -> bool {
    c.is_numeric()
}

/// `[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}`
fn email(chars: &[char], start: usize) -> Option<usize> {
    let local = run(chars, start, |c| {
        c.is_ascii_alphanumeric() || "._%+-".contains(c)
    });
    let at = start + local;
    if local == 0 || !is(chars, at, |c| c == '@') {
        return None;
    }
    let domain = run(chars, at + 1, |c| {
        c.is_ascii_alphanumeric() || ".-".contains(c)
    });
    // the domain gives back characters until a dot with two letters after it ends the match
    (at + 2..at + 1 + domain).rev().find_map(|dot| {
        let letters = run(chars, dot + 1, |c| c.is_ascii_alphabetic());
        (chars[dot] == '.' && letters >= 2).then_some(dot + 1 + letters - start)
    })
}

/// `\+?\d[\d ()\-]{7,}\d`
fn phone(chars: &[char], start: usize) -> Option<usize> {
    let first = start + usize::from(is(chars, start, |c| c == '+'));
    if !is(chars, first, digit) {
        return None;
    }
    let middle = run(chars, first + 1, |c| digit(c) || " ()-".contains(c));
    // the run gives back characters until a digit ends it, seven or more after the first
    (first + 8..first + 1 + middle)
        .rev()
        .find(|&last| digit(chars[last]))
        .map(|last| last + 1 - start)
}

/// `(?:\d[ -]?){13,19}`
fn card(chars: &[char], start: usize) -> Option<usize> {
    let (mut at, mut digits) = (start, 0);
    while digits < 19 && is(chars, at, digit) {
        at += 1 + usize::from(is(chars, at + 1, |c| c == ' ' || c == '-'));
        digits += 1;
    }
    (digits >= 13).then_some(at - start)
}

/// `\d{3}-\d{2}-\d{4}`
fn social_security(chars: &[char], start: usize) -> Option<usize> {
    let shape = "ddd-dd-dddd";
    (shape.chars().enumerate())
        .all(|(i, s)| {
            is(
                chars,
                start + i,
                |c| if s == 'd' { digit(c) } else { c == s },
            )
        })
        .then_some(shape.len())
}

/// `(?:sk|pk|ghp|gho|xox[bap]|AKIA|AIza)[A-Za-z0-9_\-]{10,}`
fn api_key(chars: &[char], start: usize) -> Option<usize> {
    let prefixes = [
        "sk", "pk", "ghp", "gho", "xoxb", "xoxa", "xoxp", "AKIA", "AIza",
    ];
    prefixes.iter().find_map(|prefix| {
        let length = prefix.chars().count();
        let starts = (prefix.chars().enumerate()).all(|(i, p)| is(chars, start + i, |c| c == p));
        let key = run(chars, start + length, |c| {
            c.is_ascii_alphanumeric() || "_-".contains(c)
        });
        (starts && key >= 10).then_some(length + key)
    })
}

/// `[A-Za-z0-9+/_\-]{32,}={0,2}`
fn token(chars: &[char], start: usize) -> Option<usize> {
    let body = run(chars, start, |c| {
        c.is_ascii_alphanumeric() || "+/_-".contains(c)
    });
    let padding = run(chars, start + body, |c| c == '=').min(2);
    (body >= 32).then_some(body + padding)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(text: &str) -> bool {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 8.0,
        };
        let words = [Word {
            text: text.to_owned(),
            rect,
            line: 0,
        }];
        let hits = find_pii(&words);
        if let Some(hit) = hits.first() {
            let grown = Rect {
                x: 7.0,
                y: 17.0,
                width: 36.0,
                height: 14.0,
            };
            assert_eq!(*hit, grown, "{text}");
        }
        !hits.is_empty()
    }

    #[test]
    fn an_email_address_with_a_dot_and_two_letters_after_its_at() {
        assert!(found("jane.doe+news@mail.example.org"));
        assert!(found("(jane@example.io)"), "two characters of punctuation");
        assert!(!found("jane@example.c"), "one letter after the last dot");
        assert!(!found("@example.com"), "nothing before the at");
        assert!(!found("Newsletter"));
    }

    #[test]
    fn a_phone_number_of_nine_digits_and_spaces_or_more() {
        assert!(found("+1(555)123-4567"));
        assert!(found("030 1234567"));
        assert!(!found("555-1234"), "seven digits and a dash");
        assert!(!found("Tuesday"));
    }

    #[test]
    fn a_card_number_of_thirteen_to_nineteen_digits() {
        assert!(found("4111-1111-1111-1111"));
        assert!(found("4111111111111"));
        // a digit and a separator at a time; a long run of digits passes as a phone number
        assert!(!found("4111.1111.1111.1111"), "dots between the groups");
        assert!(!found("receipt"));
    }

    #[test]
    fn a_social_security_number_in_its_dashes() {
        assert!(found("078-05-1120"));
        assert!(
            found("١٢٣-٤٥-٦٧٨٩"),
            "in Arabic-Indic digits, as a Unicode \\d takes them"
        );
        // the dashes elsewhere pass as a phone number
        assert!(!found("078/05/1120"), "slashes for dashes");
        assert!(!found("dashes"));
    }

    #[test]
    fn an_api_key_after_one_of_the_known_prefixes() {
        assert!(found("sk_live_4eC39HqLyjWDarjtT1"));
        assert!(found("AKIAIOSFODNN7EXAMPLE"));
        assert!(found("xoxb-1234567890-abc"));
        assert!(!found("skeleton"), "six characters after the prefix");
        assert!(!found("ghost"));
    }

    #[test]
    fn a_token_of_thirty_two_characters_or_more() {
        assert!(found("dGhpcyBpcyBhIHNlY3JldCB0b2tlbiE+Zm9vYmFy=="));
        assert!(!found("dGhpcyBpcyBhIHNlY3JldCB0b2tl"), "28 characters");
        assert!(!found("international"));
    }

    /// Each line's words, 100 pixels apart from x 0, 20 high and 50 apart from y 0, and their
    /// boxes found.
    fn found_in(lines: &[&str]) -> Vec<Rect> {
        let mut words = Vec::new();
        for (line, text) in lines.iter().enumerate() {
            for (column, word) in text.split(' ').enumerate() {
                let rect = Rect {
                    x: 100.0 * column as f64,
                    y: 50.0 * line as f64,
                    width: 80.0,
                    height: 20.0,
                };
                let text = word.to_owned();
                words.push(Word { text, rect, line });
            }
        }
        find_pii(&words)
    }

    /// The box around words `from` to `to` of line `line`, grown by 3.
    fn words(line: usize, from: usize, to: usize) -> Rect {
        Rect {
            x: 100.0 * from as f64 - 3.0,
            y: 50.0 * line as f64 - 3.0,
            width: 100.0 * (to - from) as f64 + 86.0,
            height: 26.0,
        }
    }

    #[test]
    fn a_number_in_groups_is_found_across_its_words() {
        assert_eq!(found_in(&["+1 555 123 4567"]), [words(0, 0, 3)]);
        assert_eq!(found_in(&["4111 1111 1111 1111"]), [words(0, 0, 3)]);
        // among other words, which stay as they are
        assert_eq!(found_in(&["call 555 123 4567 today"]), [words(0, 1, 3)]);
        assert_eq!(
            found_in(&["card 4111 1111 1111 1111 expires"]),
            [words(0, 1, 4)]
        );
        // and a word that is one alone beside it
        assert_eq!(
            found_in(&["jane@example.org or 555 123 4567"]),
            [words(0, 0, 0), words(0, 2, 4)]
        );
    }

    #[test]
    fn an_empty_word_starts_no_match_and_joins_no_box() {
        // the empty word before "hello", and one before a number, which is found without it
        assert_eq!(found_in(&[" hello"]), []);
        assert_eq!(found_in(&[" 555 123 4567"]), [words(0, 1, 3)]);
    }

    #[test]
    fn a_match_across_words_leaves_two_characters_of_them_at_most() {
        assert_eq!(found_in(&["call 555 123 4567today"]), []);
        assert_eq!(found_in(&["call xyz555 123 4567"]), []);
        // one before the match in the first word and one after it in the last
        assert_eq!(found_in(&["(555) 123 4567,"]), [words(0, 0, 2)]);
    }

    #[test]
    fn a_match_across_words_ends_at_the_last_word_it_can() {
        // the greedy match runs into "12:30pm", which it does not end; the number before it
        assert_eq!(found_in(&["call 555 123 4567 12:30pm"]), [words(0, 1, 3)]);
        // and it ends two characters short of "12pm", which a match may, as of a word alone
        assert_eq!(found_in(&["call 555 123 4567 12pm"]), [words(0, 1, 4)]);
        assert_eq!(
            found_in(&["card 4111 1111 1111 1111 12pm"]),
            [words(0, 1, 5)]
        );
    }

    #[test]
    fn a_row_of_figures_or_a_date_with_spaces_passes_as_a_phone_number() {
        assert_eq!(found_in(&["scores 1 2 3 4 5"]), [words(0, 1, 5)]);
        assert_eq!(found_in(&["on 2026 11 15"]), [words(0, 1, 3)]);
    }

    #[test]
    fn a_number_split_over_two_lines_is_not_joined() {
        assert_eq!(found_in(&["call 555", "123 4567"]), []);
    }

    #[test]
    fn a_match_short_of_the_word_by_three_characters_is_no_hit() {
        assert!(found("x078-05-1120y"), "two left over");
        assert!(!found("xx078-05-1120y"), "three left over");
    }
}
