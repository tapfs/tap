/// Built-in connector specs embedded at compile time.
///
/// These YAML specs are compiled into the binary so that `tap mount github`
/// works without requiring `--spec connectors/github.yaml` or a repo clone.
pub fn builtin_spec(name: &str) -> Option<&'static str> {
    match name {
        "github" => Some(include_str!("../../connectors/github.yaml")),
        "slack" => Some(include_str!("../../connectors/slack.yaml")),
        "salesforce" => Some(include_str!("../../connectors/salesforce.yaml")),
        "stripe" => Some(include_str!("../../connectors/stripe.yaml")),
        "notion" => Some(include_str!("../../connectors/notion.yaml")),
        "linear" => Some(include_str!("../../connectors/linear.yaml")),
        "hubspot" => Some(include_str!("../../connectors/hubspot.yaml")),
        "zendesk" => Some(include_str!("../../connectors/zendesk.yaml")),
        "pagerduty" => Some(include_str!("../../connectors/pagerduty.yaml")),
        "servicenow" => Some(include_str!("../../connectors/servicenow.yaml")),
        "jsonplaceholder" => Some(include_str!("../../connectors/jsonplaceholder.yaml")),
        "rest" => Some(include_str!("../../connectors/rest.yaml")),
        "gitlab" => Some(include_str!("../../connectors/gitlab.yaml")),
        "asana" => Some(include_str!("../../connectors/asana.yaml")),
        "clickup" => Some(include_str!("../../connectors/clickup.yaml")),
        "discord" => Some(include_str!("../../connectors/discord.yaml")),
        "mailchimp" => Some(include_str!("../../connectors/mailchimp.yaml")),
        "sendgrid" => Some(include_str!("../../connectors/sendgrid.yaml")),
        "shopify" => Some(include_str!("../../connectors/shopify.yaml")),
        "cloudflare" => Some(include_str!("../../connectors/cloudflare.yaml")),
        _ => None,
    }
}

pub fn builtin_names() -> &'static [&'static str] {
    &[
        "github",
        "google",     // native connector, no YAML spec
        "jira",       // native connector, no YAML spec
        "confluence", // native connector, no YAML spec
        "slack",
        "salesforce",
        "stripe",
        "notion",
        "linear",
        "hubspot",
        "zendesk",
        "pagerduty",
        "servicenow",
        "jsonplaceholder",
        "rest",
        "gitlab",
        "asana",
        "clickup",
        "discord",
        "mailchimp",
        "sendgrid",
        "shopify",
        "cloudflare",
    ]
}
