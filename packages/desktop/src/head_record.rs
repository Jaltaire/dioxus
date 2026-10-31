#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeadInsertion {
    PutIn,
    AlreadyInPage,
}

#[derive(Debug, Default)]
pub(crate) struct HeadRecord {
    scripts: Vec<String>,
    replayed: Vec<String>,
}

impl HeadRecord {
    pub(crate) fn remember(&mut self, script: String) -> HeadInsertion {
        if let Some(place) = self
            .replayed
            .iter()
            .position(|replayed| *replayed == script)
        {
            self.replayed.remove(place);
            return HeadInsertion::AlreadyInPage;
        }
        if !self.scripts.contains(&script) {
            self.scripts.push(script);
        }
        HeadInsertion::PutIn
    }

    #[cfg(test)]
    pub(crate) fn scripts(&self) -> Vec<String> {
        self.scripts.clone()
    }

    pub(crate) fn replay(&mut self) -> Vec<String> {
        self.replayed = self.scripts.clone();
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
        assert!(HeadRecord::default().replay().is_empty());
    }

    #[test]
    fn a_first_page_puts_in_every_element_it_is_asked_for() {
        let mut record = HeadRecord::default();
        assert_eq!(record.remember("link".to_string()), HeadInsertion::PutIn);
        assert_eq!(record.remember("style".to_string()), HeadInsertion::PutIn);
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::PutIn,
            "A first page is not second-guessed: what is asked for twice is put in twice, as \
             it always was."
        );
    }

    #[test]
    fn an_element_the_replay_put_in_is_not_put_in_again_by_its_remounted_component() {
        let mut record = HeadRecord::default();
        record.remember("link".to_string());
        record.remember("style".to_string());
        assert_eq!(
            record.replay(),
            vec!["link".to_string(), "style".to_string()]
        );
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::AlreadyInPage,
            "The remounted style found its element already in the page."
        );
        assert_eq!(
            record.scripts(),
            vec!["link".to_string(), "style".to_string()],
            "Claiming a replayed element leaves the record as it was."
        );
    }

    #[test]
    fn a_replayed_element_is_claimed_once_so_a_second_copy_asked_for_is_put_in() {
        let mut record = HeadRecord::default();
        record.remember("style".to_string());
        record.replay();
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::AlreadyInPage
        );
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::PutIn,
            "A second component asking for the same element gets it, as on a first page."
        );
    }

    #[test]
    fn an_element_new_to_the_replacing_page_is_put_in_and_remembered() {
        let mut record = HeadRecord::default();
        record.remember("style".to_string());
        record.replay();
        assert_eq!(record.remember("meta".to_string()), HeadInsertion::PutIn);
        assert_eq!(
            record.scripts(),
            vec!["style".to_string(), "meta".to_string()]
        );
    }

    #[test]
    fn each_replay_starts_the_claims_over() {
        let mut record = HeadRecord::default();
        record.remember("link".to_string());
        record.remember("style".to_string());
        record.replay();
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::AlreadyInPage
        );
        record.replay();
        assert_eq!(
            record.remember("style".to_string()),
            HeadInsertion::AlreadyInPage,
            "The next replacement page got the style from its own replay."
        );
        assert_eq!(
            record.remember("link".to_string()),
            HeadInsertion::AlreadyInPage
        );
    }
}
