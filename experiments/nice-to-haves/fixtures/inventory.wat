;; A type-only component fixture: no executable guest and no publication.
(component
  (type $message (record (field "text" string)))
  (import "message" (type $imported-message (eq $message)))
  (import "echo" (func $echo (param "message" $imported-message) (result string)))
  (export "echo" (func $echo))
  (type $api (instance
    (export "ping" (func (param "value" u32) (result u32)))))
  (import "example:fixture/api@1.0.0" (instance $api-import (type $api)))
  (export "example:fixture/api@1.0.0" (instance $api-import))
  (component $nested)
)
