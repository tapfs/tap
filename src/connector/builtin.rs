/// Built-in connector specs embedded at compile time.
///
/// These YAML specs are compiled into the binary so that `tap mount github`
/// works without requiring `--spec connectors/github.yaml` or a repo clone.

pub fn builtin_spec(name: &str) -> Option<&'static str> {
    match name {
        "github" => Some(include_str!("../../connectors/github.yaml")),
        "google" => Some(include_str!("../../connectors/google.yaml")),
        "jira" => Some(include_str!("../../connectors/jira.yaml")),
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
        _ => None,
    }
}

pub fn builtin_names() -> &'static [&'static str] {
    &[
        "github",
        "google",
        "jira",
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
    ]
}
