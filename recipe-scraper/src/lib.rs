use std::collections::HashMap;

use scraper::{Html, Selector};

use serde::{Deserialize, Serialize};
use serde_json::Value;
mod http_utils;
mod ld_schema;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ScrapeError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("could not find ld+json for `{0}`")]
    NoLDJSON(String),
    #[error("could not find recipe in ld ld+json for `{0}`")]
    LDJSONMissingRecipe(String),
    #[error("could not deserialize `{0}`")]
    Deserialize(#[from] serde_json::Error),
    #[error("could not parse `{0}`")]
    Parse(String),
}
#[derive(Debug, Deserialize, Serialize)]
pub struct ScrapedRecipe {
    pub ingredients: Vec<String>,
    pub instructions: Vec<String>,
    pub name: String,
    pub url: String,
    pub image: Option<String>,
}
// inspiration
// https://github.com/pombadev/sunny/blob/main/src/lib/spider.rs
// https://github.com/megametres/recettes-api/blob/dev/src/html_parser/mod.rs

#[derive(Debug)]
pub struct Scraper {
    client: reqwest_middleware::ClientWithMiddleware,
    cache: Option<HashMap<String, String>>,
}
impl Scraper {
    pub fn new() -> Self {
        return Scraper {
            client: http_utils::http_client(),
            cache: None,
        };
    }
    pub fn new_with_cache(m: HashMap<String, String>) -> Self {
        return Scraper {
            client: http_utils::http_client(),
            cache: Some(m),
        };
    }
    #[tracing::instrument(name = "scrape_url")]
    pub async fn scrape_url(&self, url: &str) -> Result<ScrapedRecipe, ScrapeError> {
        let body = self.fetch_html(url).await?;
        scrape(body.as_ref(), url)
    }

    #[tracing::instrument]
    async fn fetch_html(&self, url: &str) -> Result<String, ScrapeError> {
        if let Some(cache) = &self.cache {
            if let Some(cached) = cache.get(url) {
                return Ok(cached.to_string());
            }
        }

        let r = match self
            .client
            .get(url)
            .header("user-agent", "recipe")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    reqwest_middleware::Error::Middleware(e) => panic!("{}", e),
                    reqwest_middleware::Error::Reqwest(e) => ScrapeError::Http(e),
                })
            }
        };
        if !r.status().is_success() {
            let e = Err(ScrapeError::Http(r.error_for_status_ref().unwrap_err()));
            dbg!(r.text().await?);
            return e;
        }
        Ok(r.text().await?)
    }
}
pub fn scrape(body: &str, url: &str) -> Result<ScrapedRecipe, ScrapeError> {
    let dom = Html::parse_document(body);
    match extract_ld(dom.clone()) {
        Ok(ld_schema) => scrape_from_json(ld_schema.as_str(), url),
        Err(e) => match e {
            ScrapeError::NoLDJSON(_) => scrape_from_html(dom),
            _ => Err(e),
        },
    }
}

fn scrape_from_json(json: &str, url: &str) -> Result<ScrapedRecipe, ScrapeError> {
    normalize_ld_json(parse_ld_json(json.to_owned())?, url)
}

#[tracing::instrument]
fn normalize_root_recipe(ld_schema: ld_schema::RootRecipe, url: &str) -> ScrapedRecipe {
    ScrapedRecipe {
        ingredients: ld_schema.recipe_ingredient,
        instructions: match ld_schema.recipe_instructions {
            ld_schema::InstructionWrapper::A(a) => a.into_iter().map(|i| i.text).collect(),
            ld_schema::InstructionWrapper::B(b) => b
                .clone()
                .pop()
                .unwrap()
                .item_list_element
                .iter()
                .map(|i| i.text.clone().unwrap())
                .collect(),
            ld_schema::InstructionWrapper::C(c) => {
                let selector = Selector::parse("p").unwrap();

                let foo = Html::parse_fragment(c.as_ref())
                    .select(&selector)
                    .map(|i| i.text().collect::<Vec<_>>().join(""))
                    .collect::<Vec<_>>();
                foo
                // c.split("</p>\n, <p>").map(|s| s.into()).collect()
            }
            ld_schema::InstructionWrapper::D(d) => {
                d[0].clone().into_iter().map(|i| i.text).collect()
            }
        },

        name: ld_schema.name,
        url: url.to_string(),
        image: match ld_schema.image {
            Some(image) => match image {
                ld_schema::ImageOrList::URL(i) => Some(i),
                ld_schema::ImageOrList::List(l) => Some(l[0].url.clone()),
                ld_schema::ImageOrList::URLList(i) => Some(i[0].clone()),
                ld_schema::ImageOrList::Image(i) => Some(i.url),
            },
            None => None,
        },
    }
}
#[tracing::instrument]
fn normalize_ld_json(
    ld_schema_a: ld_schema::Root,
    url: &str,
) -> Result<ScrapedRecipe, ScrapeError> {
    match ld_schema_a {
        ld_schema::Root::Recipe(ld_schema) => Ok(normalize_root_recipe(ld_schema, url)),
        ld_schema::Root::Graph(g) => {
            let recipe = g.graph.iter().find_map(|d| match d {
                ld_schema::Graph::Recipe(a) => Some(a.to_owned()),
                _ => None,
            });
            match recipe {
                Some(r) => Ok(normalize_root_recipe(r, url)),
                None => Err(ScrapeError::LDJSONMissingRecipe(
                    "failed to find recipe in ld json".to_string(),
                )),
            }
        }
    }
}
fn scrape_from_html(dom: Html) -> Result<ScrapedRecipe, ScrapeError> {
    // smitten kitchen
    let ingredient_selector = Selector::parse("li.jetpack-recipe-ingredient").unwrap();
    let ingredients = dom
        .select(&ingredient_selector)
        .map(|i| i.text().collect::<Vec<_>>().join(""))
        .collect::<Vec<String>>();

    let ul_selector = Selector::parse(r#"div.jetpack-recipe-directions"#).unwrap();

    let foo = match dom.select(&ul_selector).next() {
        Some(x) => x,
        None => return Err(ScrapeError::Parse("no ld json or parsed html".to_string())),
    };

    let instructions = foo
        .text()
        .collect::<Vec<_>>()
        .join("")
        .split("\n")
        .map(|s| s.into())
        .collect::<Vec<String>>();

    Ok(dbg!(ScrapedRecipe {
        ingredients,
        instructions,
        name: "".to_string(),
        url: "".to_string(),
        image: None,
    }))
    // Err(ScrapeError::Parse("foo".to_string()))
}
fn extract_ld(dom: Html) -> Result<String, ScrapeError> {
    let selector = match Selector::parse("script[type='application/ld+json']") {
        Ok(s) => s,
        Err(e) => return Err(ScrapeError::Parse(format!("{:?}", e))),
    };

    let element = match dom.select(&selector).next() {
        Some(e) => e,
        None => {
            return Err(ScrapeError::NoLDJSON(
                dom.root_element().html().chars().take(40).collect(),
            ))
        }
    };

    Ok(element.inner_html())
}
fn parse_ld_json(json: String) -> Result<ld_schema::Root, ScrapeError> {
    let json = json.as_str();
    let _raw = serde_json::from_str::<Value>(json)?;
    // dbg!(_raw);
    // tracing::info!("raw json: {:#?}", raw);
    let v: ld_schema::Root = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return Err(ScrapeError::Deserialize(e)),
    };

    return Ok(v);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{ld_schema::InstructionWrapper, Scraper};

    macro_rules! include_testdata {
        ($x:expr) => {
            include_str!(concat!("../test_data/", $x))
        };
    }

    fn get_scraper() -> Scraper {
        Scraper::new_with_cache(HashMap::from([
            (
                "https://cooking.nytimes.com/recipes/1015819-chocolate-chip-cookies".to_string(),
                include_testdata!("nytimes_chocolate_chip_cookies.html").to_string(),
            ),
            (
                "http://www.seriouseats.com/recipes/2011/08/grilled-naan-recipe.html".to_string(),
                include_testdata!("seriouseats_grilled_naan.html").to_string(),
            ),
            (
                "https://www.kingarthurbaking.com/recipes/pretzel-focaccia-recipe".to_string(),
                include_testdata!("kingarthurbaking_pretzel-focaccia-recipe.html").to_string(),
            ),
            (
                "https://smittenkitchen.com/2018/04/crispy-tofu-pad-thai/".to_string(),
                include_testdata!("smittenkitchen_crispy-tofu-pad-thai.html").to_string(),
            ),
        ]))
    }

    #[tokio::test]
    async fn scrape_from_live() {
        let res = get_scraper()
            .scrape_url("http://cooking.nytimes.com/recipes/1017060-doughnuts")
            .await
            .unwrap();
        assert_eq!(res.ingredients.len(), 8);
    }

    #[tokio::test]
    async fn scrape_from_cache() {
        let res = get_scraper()
            .scrape_url("https://cooking.nytimes.com/recipes/1015819-chocolate-chip-cookies")
            .await
            .unwrap();
        assert_eq!(res.ingredients.len(), 12);

        let res = get_scraper()
            .scrape_url("http://www.seriouseats.com/recipes/2011/08/grilled-naan-recipe.html")
            .await
            .unwrap();
        assert_eq!(res.ingredients.len(), 6);

        let res = get_scraper()
            .scrape_url("https://www.kingarthurbaking.com/recipes/pretzel-focaccia-recipe")
            .await
            .unwrap();
        assert_eq!(res.ingredients.len(), 14);
        assert_eq!(res.instructions[0], "To make the starter: Mix the water and yeast. Weigh your flour; or measure it by gently spooning it into a cup, then sweeping off any excess. Add the flour, stirring until the flour is incorporated. The starter will be paste-like; it won't form a ball.");
    }
    #[tokio::test]
    async fn scrape_from_cache_html() {
        let res = get_scraper()
            .scrape_url("https://smittenkitchen.com/2018/04/crispy-tofu-pad-thai/")
            .await
            .unwrap();
        assert_eq!(res.ingredients.len(), 17);
        assert_eq!(res.instructions.len(), 16);
    }
    #[test]
    fn json() {
        assert_eq!(
            crate::parse_ld_json(include_testdata!("empty.json").to_string()).unwrap(),
            crate::ld_schema::Root::Recipe(crate::ld_schema::RootRecipe {
                context: None,
                name: "".to_string(),
                image: None,
                recipe_ingredient: vec![],
                recipe_instructions: InstructionWrapper::A(vec![]),
            })
        );
        let r = crate::scrape_from_json(
            include_testdata!("diningwithskyler_carbone-spicy-rigatoni-vodka.json"),
            "a".as_ref(),
        )
        .unwrap();
        assert_eq!(r.ingredients.len(), 11);
        assert_eq!(r.instructions.len(), 9); // todo

        let r = crate::scrape_from_json(
            include_testdata!("thewoksoflife_vietnamese-rice-noodle-salad-chicken.json"),
            "a".as_ref(),
        )
        .unwrap();
        assert_eq!(r.instructions.len(), 5);
        assert_eq!(r.ingredients.len(), 22);
    }

    #[test]
    fn handle_no_ldjson() {
        assert!(matches!(
            crate::scrape(include_testdata!("missing.html"), "https://missing.com",).unwrap_err(),
            crate::ScrapeError::Parse(_)
        ));

        assert!(matches!(
            crate::scrape(include_testdata!("malformed.html"), "https://malformed.com",)
                .unwrap_err(),
            crate::ScrapeError::Parse(_)
        ));
    }
    #[tokio::test]
    async fn scrape_errors() {
        assert!(matches!(
            get_scraper()
                .scrape_url("https://doesnotresolve.com")
                .await
                .unwrap_err(),
            crate::ScrapeError::Http(_)
        ));
    }
}
