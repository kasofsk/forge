# ActionRun

## Properties

Name | Type | Description | Notes
------------ | ------------- | ------------- | -------------
**schedule_id** | Option<**i64**> | the cron id for the schedule trigger | [optional]
**approved_by** | Option<**i64**> | who approved this action run | [optional]
**commit_sha** | Option<**String**> | the commit sha the action run ran on | [optional]
**created** | Option<**String**> | when the action run was created | [optional]
**duration** | Option<**i64**> | A Duration represents the elapsed time between two instants as an int64 nanosecond count. The representation limits the largest representable duration to approximately 290 years. | [optional]
**event** | Option<**String**> | the webhook event that causes the workflow to run | [optional]
**event_payload** | Option<**String**> | the payload of the webhook event that causes the workflow to run | [optional]
**html_url** | Option<**String**> | the url of this action run | [optional]
**id** | Option<**i64**> | the action run id | [optional]
**index_in_repo** | Option<**i64**> | a unique number for each run of a repository | [optional]
**is_fork_pull_request** | Option<**bool**> | If this is triggered by a PR from a forked repository or an untrusted user, we need to check if it is approved and limit permissions when running the workflow. | [optional]
**is_ref_deleted** | Option<**bool**> | has the commit/tag/… the action run ran on been deleted | [optional]
**need_approval** | Option<**bool**> | may need approval if it's a fork pull request | [optional]
**prettyref** | Option<**String**> | the commit/tag/… the action run ran on | [optional]
**repository** | Option<[**models::Repository**](Repository.md)> |  | [optional]
**started** | Option<**String**> | when the action run was started | [optional]
**status** | Option<**String**> | the current status of this run | [optional]
**stopped** | Option<**String**> | when the action run was stopped | [optional]
**title** | Option<**String**> | the action run's title | [optional]
**trigger_event** | Option<**String**> | the trigger event defined in the `on` configuration of the triggered workflow | [optional]
**trigger_user** | Option<[**models::User**](User.md)> |  | [optional]
**updated** | Option<**String**> | when the action run was last updated | [optional]
**workflow_id** | Option<**String**> | the name of workflow file | [optional]

[[Back to Model list]](../README.md#documentation-for-models) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to README]](../README.md)


