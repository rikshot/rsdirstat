//! What drawing the treemap needs of the page's look, copied from `main.css` and `index.html`, which
//! the tests below keep these in step with. The canvas is sized from the window rather than measured,
//! because it is sized while the treemap view can still be hidden, when its bars measure 0.

/// `--bg`, which the canvas is cleared to.
pub(crate) const BACKGROUND: &str = "#1a1a2e";
/// `--font`, for labels.
pub(crate) const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";
/// The heights of `#breadcrumb-bar` and `#toolbar`, above the canvas, and of `#path-bar`, below it.
pub(crate) const BREADCRUMB_HEIGHT: f64 = 32.0;
pub(crate) const TOOLBAR_HEIGHT: f64 = 28.0;
pub(crate) const PATH_BAR_HEIGHT: f64 = 24.0;

#[cfg(test)]
mod tests {
    use rsdirstat_protocol::{DEFAULT_DEPTH, MAX_DEPTH};

    use super::*;

    const CSS: &str = include_str!("../main.css");
    const HTML: &str = include_str!("../index.html");

    /// The value of `property` in the `main.css` rule for `selector` itself (not in a rule nested in
    /// it), with its whitespace collapsed.
    fn css_value(selector: &str, property: &str) -> String {
        let css = format!("\n{CSS}");
        let rule = format!("\n{selector} {{");
        let start = css.find(&rule).unwrap_or_else(|| panic!("no rule for {selector}")) + rule.len();
        let mut depth = 1;
        let mut own = String::new();
        for c in css[start..].chars() {
            match c {
                '{' => depth += 1,
                '}' if depth == 1 => break,
                '}' => depth -= 1,
                _ if depth == 1 => own.push(c),
                _ => {}
            }
        }
        own.split(';')
            .find_map(|declaration| {
                declaration
                    .trim()
                    .strip_prefix(property)?
                    .trim_start()
                    .strip_prefix(':')
            })
            .unwrap_or_else(|| panic!("no {property} for {selector}"))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_constants_match_the_stylesheet() {
        assert_eq!(css_value(":root", "--bg"), BACKGROUND);
        assert_eq!(
            css_value(":root", "--font"),
            FONT.replace(',', ", ").replace('\'', "\"")
        );
        for (selector, height) in [
            ("#breadcrumb-bar", BREADCRUMB_HEIGHT),
            ("#toolbar", TOOLBAR_HEIGHT),
            ("#path-bar", PATH_BAR_HEIGHT),
        ] {
            assert_eq!(css_value(selector, "height"), format!("{height}px"), "{selector}");
        }
    }

    /// The page's Depth menu offers every depth the server takes, with its default selected.
    #[test]
    fn the_depth_menu_matches_the_protocol() {
        let start = HTML.find(r#"<select id="depth">"#).expect("no depth menu");
        let menu = &HTML[start..start + HTML[start..].find("</select>").unwrap()];
        let options: Vec<(bool, u8)> = menu
            .split("<option")
            .skip(1)
            .map(|option| {
                let (attributes, rest) = option.split_once('>').unwrap();
                (
                    attributes.contains("selected"),
                    rest.split('<').next().unwrap().parse().unwrap(),
                )
            })
            .collect();
        let depths: Vec<u8> = options.iter().map(|&(_, depth)| depth).collect();
        assert_eq!(depths, (1..=MAX_DEPTH).collect::<Vec<_>>());
        let selected: Vec<u8> = options
            .iter()
            .filter(|&&(selected, _)| selected)
            .map(|&(_, depth)| depth)
            .collect();
        assert_eq!(selected, [DEFAULT_DEPTH]);
    }
}
