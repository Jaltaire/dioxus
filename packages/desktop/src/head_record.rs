#[derive(Debug, Default)]
pub(crate) struct HeadRecord {
    scripts: Vec<String>,
}

impl HeadRecord {
    pub(crate) fn remember(&mut self, script: String) {
        if !self.scripts.contains(&script) {
            self.scripts.push(script);
        }
    }

    pub(crate) fn scripts(&self) -> Vec<String> {
        self.scripts.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_element_is_kept_once_in_the_order_it_was_first_put_in() {
        let mut record = HeadRecord::default();
        record.remember("link".to_string());
        record.remember("style".to_string());
        record.remember("style".to_string());
        assert_eq!(
            record.scripts(),
            vec!["link".to_string(), "style".to_string()],
            "An element put in again by a remounted component should not be put into the next page twice."
        );
    }

    #[test]
    fn every_page_that_replaces_another_gets_the_whole_head_back() {
        let mut record = HeadRecord::default();
        record.remember("link".to_string());
        record.remember("style".to_string());
        let first = record.scripts();

        record.remember("style".to_string());
        let second = record.scripts();

        assert_eq!(
            first, second,
            "The second page to replace another should get the same head as the first."
        );
        assert!(
            second.contains(&"link".to_string()),
            "An element its component will not put in again, such as a de-duplicated stylesheet \
             link, should still reach the second replacement."
        );
    }

    #[test]
    fn a_page_that_replaced_nothing_has_nothing_to_get_back() {
        assert!(HeadRecord::default().scripts().is_empty());
    }
}
