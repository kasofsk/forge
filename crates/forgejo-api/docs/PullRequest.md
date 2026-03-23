# PullRequest

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**additions** | Option<**i64**> |  | [optional]
**allow_maintainer_edit** | Option<**bool**> |  | [optional]
**assignee** | Option<[**models::User**](User.md)> |  | [optional]
**assignees** | Option<[**Vec<models::User>**](User.md)> |  | [optional]
**base** | Option<[**models::PrBranchInfo**](PRBranchInfo.md)> |  | [optional]
**body** | Option<**String**> |  | [optional]
**changed_files** | Option<**i64**> |  | [optional]
**closed_at** | Option<**String**> |  | [optional]
**comments** | Option<**i64**> |  | [optional]
**created_at** | Option<**String**> |  | [optional]
**deletions** | Option<**i64**> |  | [optional]
**diff_url** | Option<**String**> |  | [optional]
**draft** | Option<**bool**> |  | [optional]
**due_date** | Option<**String**> |  | [optional]
**flow** | Option<**i64**> |  | [optional]
**head** | Option<[**models::PrBranchInfo**](PRBranchInfo.md)> |  | [optional]
**html_url** | Option<**String**> |  | [optional]
**id** | Option<**i64**> |  | [optional]
**is_locked** | Option<**bool**> |  | [optional]
**labels** | Option<[**Vec<models::Label>**](Label.md)> |  | [optional]
**merge_base** | Option<**String**> |  | [optional]
**merge_commit_sha** | Option<**String**> |  | [optional]
**mergeable** | Option<**bool**> |  | [optional]
**merged** | Option<**bool**> |  | [optional]
**merged_at** | Option<**String**> |  | [optional]
**merged_by** | Option<[**models::User**](User.md)> |  | [optional]
**milestone** | Option<[**models::Milestone**](Milestone.md)> |  | [optional]
**number** | Option<**i64**> |  | [optional]
**patch_url** | Option<**String**> |  | [optional]
**pin_order** | Option<**i64**> |  | [optional]
**requested_reviewers** | Option<[**Vec<models::User>**](User.md)> |  | [optional]
**requested_reviewers_teams** | Option<[**Vec<models::Team>**](Team.md)> |  | [optional]
**review_comments** | Option<**i64**> | number of review comments made on the diff of a PR review (not including comments on commits or issues in a PR) | [optional]
**state** | Option<**String**> | StateType issue state type | [optional]
**title** | Option<**String**> |  | [optional]
**updated_at** | Option<**String**> |  | [optional]
**url** | Option<**String**> |  | [optional]
**user** | Option<[**models::User**](User.md)> |  | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


