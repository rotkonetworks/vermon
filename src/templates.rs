use askama::Template;

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexTemplate {
    pub version: &'static str,
}

#[derive(Template)]
#[template(path = "docs.html")]
pub struct DocsTemplate {}
