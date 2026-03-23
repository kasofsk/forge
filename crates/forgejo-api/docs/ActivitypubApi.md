# \ActivitypubApi

All URIs are relative to */api/v1*

Method | HTTP request | Description
------------- | ------------- | -------------
[**activitypub_instance_actor**](ActivitypubApi.md#activitypub_instance_actor) | **GET** /activitypub/actor | Returns the instance's Actor
[**activitypub_instance_actor_inbox**](ActivitypubApi.md#activitypub_instance_actor_inbox) | **POST** /activitypub/actor/inbox | Send to the inbox
[**activitypub_instance_actor_outbox**](ActivitypubApi.md#activitypub_instance_actor_outbox) | **POST** /activitypub/actor/outbox | Display the outbox (always empty)
[**activitypub_person**](ActivitypubApi.md#activitypub_person) | **GET** /activitypub/user-id/{user_id} | Returns the Person actor for a user
[**activitypub_person_activity**](ActivitypubApi.md#activitypub_person_activity) | **GET** /activitypub/user-id/{user_id}/activities/{activity_id}/activity | Get a specific activity of the user
[**activitypub_person_activity_note**](ActivitypubApi.md#activitypub_person_activity_note) | **GET** /activitypub/user-id/{user_id}/activities/{activity_id} | Get a specific activity object of the user
[**activitypub_person_feed**](ActivitypubApi.md#activitypub_person_feed) | **GET** /activitypub/user-id/{user_id}/outbox | List the user's recorded activity
[**activitypub_person_inbox**](ActivitypubApi.md#activitypub_person_inbox) | **POST** /activitypub/user-id/{user_id}/inbox | Send to the inbox
[**activitypub_repository**](ActivitypubApi.md#activitypub_repository) | **GET** /activitypub/repository-id/{repository_id} | Returns the Repository actor for a repo
[**activitypub_repository_inbox**](ActivitypubApi.md#activitypub_repository_inbox) | **POST** /activitypub/repository-id/{repository_id}/inbox | Send to the inbox
[**activitypub_repository_outbox**](ActivitypubApi.md#activitypub_repository_outbox) | **POST** /activitypub/repository-id/{repository_id}/outbox | Display the outbox



## activitypub_instance_actor

> models::ActivityPub activitypub_instance_actor()
Returns the instance's Actor

### Parameters

This endpoint does not need any parameter.

### Return type

[**models::ActivityPub**](ActivityPub.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_instance_actor_inbox

> activitypub_instance_actor_inbox()
Send to the inbox

### Parameters

This endpoint does not need any parameter.

### Return type

 (empty response body)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: Not defined

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_instance_actor_outbox

> serde_json::Value activitypub_instance_actor_outbox()
Display the outbox (always empty)

### Parameters

This endpoint does not need any parameter.

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_person

> models::ActivityPub activitypub_person(user_id)
Returns the Person actor for a user

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**user_id** | **i64** | user ID of the user | [required] |

### Return type

[**models::ActivityPub**](ActivityPub.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_person_activity

> models::ActivityPub activitypub_person_activity(user_id, activity_id)
Get a specific activity of the user

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**user_id** | **i32** | user ID of the user | [required] |
**activity_id** | **i32** | activity ID of the sought activity | [required] |

### Return type

[**models::ActivityPub**](ActivityPub.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_person_activity_note

> models::ActivityPub activitypub_person_activity_note(user_id, activity_id)
Get a specific activity object of the user

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**user_id** | **i32** | user ID of the user | [required] |
**activity_id** | **i32** | activity ID of the sought activity | [required] |

### Return type

[**models::ActivityPub**](ActivityPub.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_person_feed

> serde_json::Value activitypub_person_feed(user_id)
List the user's recorded activity

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**user_id** | **i32** | user ID of the user | [required] |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_person_inbox

> activitypub_person_inbox(user_id)
Send to the inbox

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**user_id** | **i64** | user ID of the user | [required] |

### Return type

 (empty response body)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: Not defined

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_repository

> models::ActivityPub activitypub_repository(repository_id)
Returns the Repository actor for a repo

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**repository_id** | **i64** | repository ID of the repo | [required] |

### Return type

[**models::ActivityPub**](ActivityPub.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_repository_inbox

> activitypub_repository_inbox(repository_id, body)
Send to the inbox

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**repository_id** | **i64** | repository ID of the repo | [required] |
**body** | Option<**serde_json::Value**> |  |  |

### Return type

 (empty response body)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: application/json, text/plain
- **Accept**: Not defined

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)


## activitypub_repository_outbox

> serde_json::Value activitypub_repository_outbox(repository_id)
Display the outbox

### Parameters


Name | Type | Description  | Required | Notes
------------- | ------------- | ------------- | ------------- | -------------
**repository_id** | **i64** | repository ID of the repo | [required] |

### Return type

[**serde_json::Value**](serde_json::Value.md)

### Authorization

[TOTPHeader](../README.md#TOTPHeader), [AuthorizationHeaderToken](../README.md#AuthorizationHeaderToken), [SudoHeader](../README.md#SudoHeader), [BasicAuth](../README.md#BasicAuth), [AccessToken](../README.md#AccessToken), [SudoParam](../README.md#SudoParam), [Token](../README.md#Token)

### HTTP request headers

- **Content-Type**: Not defined
- **Accept**: application/json, text/html

[[Back to top]](#) [[Back to API list]](../README.md#documentation-for-api-endpoints) [[Back to Model list]](../README.md#documentation-for-models) [[Back to README]](../README.md)

