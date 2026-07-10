use super::EndpointDoc;

mod part_01;
mod part_02;
mod part_03;
mod part_04;
mod part_05;
mod part_06;
mod part_07;
mod part_08;
mod part_09;
mod part_10;

pub(super) fn all() -> Vec<EndpointDoc> {
    let mut endpoints = Vec::new();
    endpoints.extend(part_01::endpoints());
    endpoints.extend(part_02::endpoints());
    endpoints.extend(part_03::endpoints());
    endpoints.extend(part_04::endpoints());
    endpoints.extend(part_05::endpoints());
    endpoints.extend(part_06::endpoints());
    endpoints.extend(part_07::endpoints());
    endpoints.extend(part_08::endpoints());
    endpoints.extend(part_09::endpoints());
    endpoints.extend(part_10::endpoints());
    endpoints
}
